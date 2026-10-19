# audiosync-airplay implementation notes

Everything in this crate is written from `docs/AIRPLAY_SPEC.md`, whose clause IDs (`P1` and up) appear in comments and test names, from the RFCs it cites (2326 RTSP, 2617 digest authentication, 2945 and 5054 SRP, 3550 RTP, 3551 the audio profile, 4566 SDP, 5905 NTP timestamps, 6762 and 6763 mDNS and DNS-SD, 7748 X25519, 8032 Ed25519, 5737 and 3849 documentation addresses), from Apple's public description of the ALAC configuration record, from the author's own knowledge of the ALAC frame syntax for the uncompressed frame (see "Audio, RTP and timing"), and from the docs.rs and crates.io pages of the crates below (for `mdns-sd`, its public API and README; for the crypto crates, their documentation as docs.rs renders it from the published crate). `docs/AIRPLAY_SURVEY.md` was read to choose crates. No source code of any AirPlay sender or receiver was opened, no ALAC codec source either, and nothing else in this repository was used. The one exception to "the spec only" is data: the Apple TV pairing write-up's test vector (see "Apple TV pairing").

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

- `discovery::Browser` and `discovery::Resolver`: a live mDNS browser (now `discovery::mdns::MdnsBrowser`, see the transport section).
- `session::Verifier`: the legacy pair-verify (P55, P56). The session runs it in the right place and maps a non-200 answer to PairingRequired; `pairing::LegacyVerifier` implements it.
- `SessionConfig::auth_setup_key`: the public key of `POST /auth-setup` (P60), made by `pairing::auth_setup_key`.
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

## Transport, discovery and the host API

### Layout

- `transport.rs` (crate-private): a connect attempt on a thread of its own, and `SessionIo`, one session's UDP sockets and threads.
- `sender.rs`: the host API: `Sender`, `SenderConfig`, `SpeakerOptions`, `NowPlaying`, `ReceiverInfo` and the events.
- `sender/driver.rs`: the driver loop that owns the stream, the sessions and the directory.
- `reconnect.rs`: the retry schedule of P142 (`Backoff`).
- `discovery/mdns.rs`: `MdnsBrowser`, the live browser behind the `Browser` seam, and `browse_event`, the conversion it uses.

### Design

Plain `std::thread` and `std::net`, no async runtime. Per session: a connect thread per attempt (detached; a result nobody waits for is dropped, which closes it), then four threads once connected: the timing thread answers timing requests itself; the control thread forwards retransmission requests; the RTSP reader forwards bytes and the close; the RTSP writer writes the queued requests in 16 KiB pieces, so a receiver that stops reading blocks only that thread. The UDP and TCP readers wake every 50 ms to look at their stop flag, and the writer looks at it between pieces, so every thread ends within 50 ms of its session's close, with one exception: a writer blocked inside a send. Linux and macOS wake it when the socket is shut down; Windows doesn't, and it ends only at its 10 s send timeout. Nothing waits for it (see P129). The `mdns-sd` daemon runs one thread of its own.

One driver thread owns the `Stream`, every `Session` and the `Directory`. Each turn runs the timers that are due (stream, sessions, retries, discovery every 20 ms), then hands every output where it goes until nothing more comes: `Output::Connect` to a connect thread, `Output::Send` to the writer, `Transmit`s to the session's sockets (with the send result back to `Session::handle_udp_send`), `StreamEvent::Flush` to `Session::flush`, and events to the host. Then it waits for the next deadline: a wait under 20 ms is a plain `thread::sleep`, which is precise to well under a millisecond on every platform (Windows included, where the standard library uses a high-resolution timer); a longer one waits on the message channel first, whose wake-up some platforms round to a coarse tick, and sleeps the rest. The stream's packets are due about every 8 ms, so in practice the driver sleeps precisely; a message waits at most until the next packet. At most 256 messages are handled per turn. Locally on Windows the loopback tests send every packet on time (no late packets).

The host side: `Sender::start` creates the stream on the calling thread (so its buffers exist before audio flows) and gives back the `InputWriter`; every other call sends a message and returns. `Sender::arrival_time` is the one call that waits for an answer, at most 1 s. Events come back on a channel the host drains with `poll_event` or `wait_event`. `add_speaker` returns the speaker's `SpeakerCounters`, which stay valid across its reconnects.

### Spec readings

- **P3, P141**: a failing session releases its sockets and threads and leaves the stream; nothing it does can block the driver (writes go through its writer thread, UDP sends have a 20 ms limit). UDP send errors that say ConnectionRefused, ConnectionReset, HostUnreachable or NetworkUnreachable count as unreachable for P141; other send errors are ignored. Windows reports an ICMP unreachable as a reset on the next call on the socket, which is the control thread's receive rather than the send that caused it; the control thread notes the reset and the next send from the control socket counts as unreachable, whatever the call returned, so a receiver whose ports are gone is lost after two sends (a stray ICMP followed by a good send isn't). Linux and macOS report no ICMP errors on an unconnected UDP socket, so there only send errors count, and a vanished receiver is caught by its TCP connection or the timing silence of P131 instead.
- **P6**: the host's capture thread only touches the `InputWriter`; the host's other calls are channel sends. All network work, connects included, runs on the crate's threads.
- **P7**: `mdns-sd` follows PTR to SRV, TXT and addresses itself, so `MdnsBrowser` turns its resolved services into `BrowseEvent`s directly and the raw-record `Resolver` stays for browsers that hand over records. Addresses are sorted so a repeated resolution with the same set isn't reported as a change; TXT strings are rebuilt from the daemon's properties in their order (it keeps the first of duplicate keys, as P11 does); instance names are unescaped as RFC 6763 §4.3 writes them; an IPv6 link-local address takes the interface index as its scope; a service without an address is skipped until it has one. The daemon binds UDP 5353 with address reuse (and port reuse on Unix), so it shares the port with the system's responder; it only queries. Its channels are bounded and block the daemon when full, so the driver drains them every 20 ms and the browser drops its receivers before it shuts the daemon down.
- **P31, P82**: the control and timing sockets bind to the connection's own local address (its family and interface, with the IPv6 scope) on port 0, and all four threads run before `Session::handle_connected`, so both sockets answer before OPTIONS, let alone SETUP. A bind failure counts as a failed connect to that address, and the session moves to the next one.
- **P91**: timing requests are answered on the timing socket's own thread: the receive time is read right after `recv_from`, the transmit time right before `send_to`, and nothing but 32 bytes of formatting sits between them. The driver only hears about it afterwards (for P131), so its work never delays a reply. Requests are answered whatever their source, to that source.
- **P129, P35**: "within 2 s overall" for stopping the stream is the stricter limit, so it wins over each session's "after 2 s without one": shutdown calls `Session::teardown` on every session in the same turn, closes whatever is still open 1.75 s after the call, waits at most 100 ms for the threads to end, and leaves a writer still blocked in a send to end by itself (it holds nothing but its socket). The whole shutdown, threads included, so stays within 2 s; a session waits at most 1.75 s for its TEARDOWN answer during it. Removing one speaker keeps the session's own 2 s and never waits for threads.
- **P142**: the schedule counts the failures since the speaker last played: 1, 2, 4, 8, 16, then 30 s; playing again starts it over. Every failure whose kind doesn't need the user is retried, also for a speaker that never reached RECORD (unreachable when added). A PCM refusal with an ALAC fallback (P74) starts the ALAC session at once and doesn't count. `add_speaker` on a speaker that failed or waits for a retry starts a session at once with a fresh schedule. A speaker whose device can't be driven (P139) or needs the legacy pairing (P46) is reported without connecting, and isn't retried.
- **P144**: the counters belong to the speaker the host added and run on across its sessions, so the host keeps one handle; the timing thread counts its replies into the same counters.
- **P46**: an Apple TV that needs the legacy pairing and has no credentials is reported PairingRequired without connecting; with `SpeakerOptions::credentials` it connects and verifies. Every Apple TV without credentials gets `apple_tv_without_credentials`.
- **P60**: `POST /auth-setup` carries the X25519 public key of 32 fresh random bytes, drawn per session (so per connection) and thrown away at once.
- **P18, P19**: a discovery update changes the device the next session of a speaker uses (addresses, codec, TXT), never the running one. Stopping discovery reports every listed receiver gone; the directory keeps what it learned about PCM refusals.
- **P124 to P126**: what is playing is one value for the stream. Each item applies from the speaker's RTP timestamp of the first frame of the next packet to be cut, the audio arriving now (`Stream::rtptime_now`), and is sent again when a speaker (re)starts. Progress takes the elapsed time and the length and becomes start, current and end around that timestamp.
- **Tests**: the integration tests run whole sessions over real sockets against a fake receiver on loopback (127.0.0.1, and ::1 where the runner has it), every socket on port 0, the host writing numbered PCM audio in real time. They check play times read back from the sync packets, so being in step is measured end to end. Timeouts are 15 s, timing bounds are wide (shutdown 1.6 to 2.3 s, connections closed under 2 s, retries 0.9 to 2.5 s and 1.9 to 3.5 s), and the network tests of a binary run one at a time. The live mDNS test is ignored and never runs in CI. Loopback on Windows buffers megabytes for a receiver that stops reading, so no loopback test can block the writer; a unit test in the driver covers a thread that doesn't end after its close, and the closed-UDP-port test runs on Windows only, where the reset exists.

## Apple TV pairing

### Layout

- `pairing.rs`: `Pairing`, the one-time PIN pairing as a sans-I/O state machine; `Credentials` and their blob; `LegacyVerifier`, the pair-verify behind the session's `Verifier` seam; `auth_setup_key` for P60.
- `pairing/srp.rs`: SRP-6a, client side: k, x, u, A and S as RFC 5054 defines them, the 40-byte K and the M1 of P50, and M2 as RFC 2945 defines it.
- `pairing/keys.rs`: the AES-128-GCM seal of P53, the pair-verify signature and AES-128-CTR of P56, the X25519 and Ed25519 keys.
- `transport.rs`: `RtspIo`, a pairing's connection (reader and writer threads, no UDP).
- `sender.rs`, `sender/driver.rs`: `Sender::start_pairing`, `submit_pin`, `cancel_pairing`, `SenderEvent::Pairing`, and `SpeakerOptions::credentials`.

### Design

`Pairing::new(addresses, ids, client, now)` asks for a connect like a session (P17 order, 5 s per address); on the connection it sends `POST /pair-pin-start`, then the first `POST /pair-setup-pin` with the client ID, and reports `PinRequested` once B and the salt are in. `submit_pin(pin, entropy, now)` draws a, computes A, K, M1 and the expected M2, and sends A and M1; a matching M2 sends the sealed a_pub; the 200 to that hands the host `Paired(Credentials)` and closes the connection. Errors end it with `Failed { error, wrong_pin }`, a host cancel with `Stopped`. The driver runs each pairing on its own connection next to any sessions, under its own `SessionKey`, so I/O events find it the same way.

`Credentials` hold the client ID and a; the blob is a version byte (1), the 16 ID characters and the 32 secret bytes (49 bytes). The crate never stores it (P54): the host does, and passes it back in `SpeakerOptions::credentials`. Secrets (a, K, the SRP intermediates x, H(I:P), g^x, the exponent and S, the derived AES keys, the X25519 secrets and shared secret) sit in `Zeroizing` buffers or are wiped on drop, the hashers and ciphers wipe their state (their `zeroize` features), credentials compare in constant time (`subtle`), and no `Debug` output shows a secret.

Crypto: every primitive comes from a crate (table below): SHA-1 and SHA-512, AES-128 in GCM (with the 16-byte nonce P53 asks for, which `aes-gcm` supports through its nonce-size parameter) and CTR mode, Ed25519 (the 32-byte a is the RFC 8032 secret key) and X25519, and constant-time modular arithmetic on 2048-bit integers (`crypto-bigint`'s Montgomery form; the exponent's bit bound is public, its value isn't). The SRP formulas themselves (k, x, u, A, S) are a few lines over those primitives, checked against RFC 5054's own test vector: no SRP crate fits, since P50's M1 hashes g unpadded and its K is 40 bytes, and the stable `srp` crate's documentation doesn't say how it pads or what its key is. The binary property lists of P47 come from the `plist` crate.

Tests: `tests/pairing_vectors.rs` checks the SRP arithmetic against RFC 5054 appendix B, the keys against RFC 8032 §7.1 and RFC 7748 §6.1, and every value of the write-up's test vector that P57 lists. `tests/pairing.rs` runs the pairing and verification against a fake Apple TV (`tests/common/atv.rs`) whose receiver side (B, S, the M1 check, M2, the GCM open, the verify signature check) is written separately on the primitive crates, never through the crate's `pairing` module; `tests/pairing_net.rs` runs the host API over loopback sockets.

Where the test vector comes from: the spec names the write-up's test vector (P57) but doesn't restate it. Its values were taken as data after the implementation was written and committed from the spec alone: an extraction from the write-up page that returned only the labelled hex values of that section, with none of its prose and none of its code example. Every value matched the implementation on the first run. The values are facts (inputs and outputs of a computation), not expression.

### Spec readings

- **P47**: one connection carries all four requests; pin-start goes on it too. Requests carry CSeq and the common headers of P22; bodies are binary plists with `Content-Type: application/x-apple-binary-plist`; pin-start has no body and no content type. Pairing requests carry no `Authorization`: the write-up shows none.
- **P49**: the client ID is made when a pairing starts (driver: 8 bytes from the OS random source). Answers are read as property lists (binary or XML, whatever `plist` reads); a missing `pk` or `salt`, an empty salt, or a B that is empty, longer than N or a multiple of N is a protocol error (RFC 5054 §2.5.4), reported before the PIN is asked for.
- **P50**: a is drawn again when A or S would start with a zero byte or u would be zero; the spec requires it for S, and doing it for A too makes PAD(A) and A the same bytes in M1 and M2. After 64 draws without a usable a the pairing fails (only a broken random source gets there). The PIN is the typed string, ASCII digits only; anything else is refused before it is used (`PinError::NotDigits`).
- **P51**: while the PIN is awaited the pairing has no timer at all; every other step has the 10 s response timeout of P134 and the 5 s connect timeout per address.
- **P6, P84**: the SRP computation runs on the driver thread when the PIN arrives: about 1.5 ms in a release build on the development PC (10 ms in a debug build), once per pairing and again in about 1 of 128 for a redraw, below the 5 ms P84 allows a send to be late. So it doesn't get a thread of its own; a session's pair-verify (one X25519 and one Ed25519 operation) costs far less.
- **P52**: M2 is checked (the spec's SHOULD): a missing `proof` is a protocol error, a wrong one (compared in constant time) ends the pairing as PairingFailed before a_pub goes out, with status 200 and the detail "M2 check failed", which a host's log tells apart from a wrong PIN. A 470 to the proof is a wrong PIN: PairingFailed with `wrong_pin: true`.
- **P53**: the published vector's IV is the incremented one; the counter only touches the last byte. The 200's body is ignored.
- **P55, P56**: `LegacyVerifier` is made per session with a fresh X25519 secret from the OS random source. A first answer that isn't exactly 96 bytes, or an atv_pub that gives an all-zero shared secret (RFC 7748 §6.1), is a malformed answer (ProtocolError). AES-128-CTR runs a 128-bit big-endian counter over the whole IV; the vector's 8 blocks can't tell it from narrower counters, and it is the usual form. The verifier is used whenever the host gives credentials, Apple TV or not.
- **P58, P136**: any non-200 answer at a pairing step ends the pairing as PairingFailed with method and status (the spec names 470; other failure statuses aren't listed, and failing is the only safe reading). A close while the PIN is awaited is PairingCancelled; a close at another step is Lost, a close while connecting Unreachable. A host cancel or a shutdown ends the pairing with `Stopped`. A verification answered with anything but 200 is PairingRequired (session.rs), which isn't retried.

## Dependencies and licenses

| Crate | Version | License | Why |
|---|---|---|---|
| md-5 | 0.11.0 | MIT OR Apache-2.0 | MD5 for digest authentication (RustCrypto) |
| getrandom | 0.4.3 | MIT OR Apache-2.0 | the OS random source for identifiers |
| digest, block-buffer, crypto-common, hybrid-array, typenum | 0.11.3, 0.12.1, 0.2.2, 0.4.15, 1.20.1 | MIT OR Apache-2.0 | md-5's dependencies |
| cfg-if, libc | 1.0.5, 0.2.191 | MIT OR Apache-2.0 | getrandom's dependencies (libc on Unix) |
| r-efi | 6.0.0 | MIT OR Apache-2.0 OR LGPL-2.1-or-later | getrandom on UEFI targets only, taken under MIT |
| mdns-sd | 0.21.5 | Apache-2.0 OR MIT | live mDNS browsing (P7): a querier on its own thread, no async runtime; without default features (no async glue, no logging). Maintained (released October 2026), supports Windows, macOS and Linux |
| flume, spin, lock_api, scopeguard | 0.12.0, 0.9.9, 0.4.14, 1.2.0 | Apache-2.0/MIT, MIT, MIT OR Apache-2.0, MIT OR Apache-2.0 | mdns-sd's channels |
| mio, socket2, socket-pktinfo, if-addrs | 1.2.4, 0.6.5, 0.4.1, 0.15.0 | MIT, MIT OR Apache-2.0, MIT, MIT OR BSD-3-Clause | mdns-sd's sockets and interface list |
| fastrand, log | 2.5.0, 0.4.34 | Apache-2.0 OR MIT, MIT OR Apache-2.0 | mdns-sd's query jitter; `log` comes through mio |
| windows-sys, windows-link | 0.61.2, 0.2.1 | MIT OR Apache-2.0 | mdns-sd's dependencies on Windows |
| wasi | 0.11.1 | Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT | mio on WASI targets only, taken under MIT |
| alac | 0.5.0 | MIT/Apache-2.0 | development only: the reference ALAC decoder of the tests, without default features, so it brings no other crate |
| sha1, sha2 | 0.11.0, 0.11.0 | MIT OR Apache-2.0 | SHA-1 for SRP (P50), SHA-512 for the pairing keys (P53, P56) (RustCrypto), without default features |
| aes, aes-gcm, ctr | 0.9.2, 0.11.1, 0.10.1 | MIT OR Apache-2.0 | AES-128, GCM with a 16-byte nonce (P53), CTR (P56) (RustCrypto); aes-gcm without its default features (no allocation helpers, no OS random source) |
| aead, cipher, ghash, polyval, universal-hash, inout, cpufeatures, cpubits, cmov, ctutils | 0.6.1, 0.5.2, 0.6.0, 0.7.3, 0.6.1, 0.2.4, 0.3.1, 0.1.1, 0.5.4, 0.4.2 | MIT OR Apache-2.0 | their dependencies |
| crypto-bigint | 0.7.5 | Apache-2.0 OR MIT | constant-time 2048-bit modular arithmetic for SRP (RustCrypto), without default features |
| num-traits, rand_core | 0.2.19, 0.10.1 | MIT OR Apache-2.0 | crypto-bigint's and the dalek crates' dependencies |
| ed25519-dalek, x25519-dalek, curve25519-dalek | 3.0.0, 3.0.0, 5.0.0 | BSD-3-Clause | Ed25519 (RFC 8032) for a_pub and the verify signature, X25519 (RFC 7748) for pair-verify and /auth-setup; without default features, with `zeroize` (and `static_secrets` for x25519) |
| ed25519, signature, subtle, fiat-crypto | 3.0.0, 3.0.0, 2.6.1, 0.3.0 | Apache-2.0 OR MIT, Apache-2.0 OR MIT, BSD-3-Clause, MIT OR Apache-2.0 OR BSD-1-Clause (taken under MIT) | the dalek crates' dependencies; `subtle` is also a direct one, for the constant-time comparison of M2 and of credentials |
| curve25519-dalek-derive, proc-macro2, quote, syn, unicode-ident, rustc_version, semver, autocfg | 0.1.1, 1.0.107, 1.0.47, 2.0.119, 1.0.27, 0.4.1, 1.0.28, 1.5.1 | MIT/Apache-2.0; unicode-ident (MIT OR Apache-2.0) AND Unicode-3.0 | build time only: curve25519-dalek's backend macro and build script, num-traits' build script |
| zeroize | 1.9.1 | Apache-2.0 OR MIT | wiping the pairing secrets |
| plist | 1.8.0 | MIT | the binary property list bodies of the pairing (P47), without default features (no serde) |
| base64, indexmap, equivalent, hashbrown, quick-xml, memchr, time, time-core, deranged, powerfmt, num-conv | 0.22.1, 2.14.2, 1.0.3, 0.17.1, 0.38.4, 2.8.3, 0.3.55, 0.1.9, 0.5.9, 0.2.1, 0.2.2 | MIT OR Apache-2.0, except quick-xml (MIT) and memchr (Unlicense OR MIT, taken under MIT) | plist's dependencies |

`deny.toml` mirrors core's allow-list (MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, Zlib, ISC, Unicode-3.0) with dev and build dependencies included; CI runs cargo-deny 0.20.2. Crates the lock file lists that no build of this crate compiles (optional features off) aren't in the table. The Windows app bundles the crate, pairing crates included, so the Windows section of `THIRD_PARTY_NOTICES.md` names its dependencies and the `THIRD-PARTY-NOTICES.txt` of each Windows build carries their license texts; the macOS app doesn't bundle it.

## Spec coverage

`tests/spec_coverage.rs` reads the spec and fails unless every clause is named by a `// covers:` line or listed in `tests/spec_pending.txt`, never both. A clause counts as covered only when everything it asks of the sender is done; a test for part of a pending clause says "Part of P6" in a plain comment. After the audio, RTP and timing stage: 144 clauses, 101 covered, 43 pending (stream and transport 8, pairing crypto 14, AirPlay 2 21). After the transport stage: 109 covered, 35 pending (pairing crypto 14, AirPlay 2 21). After the pairing stage: 123 covered, 21 pending (all AirPlay 2).
 The test also checks that the IDs run P1 to Pn without gaps and that every clause names a source.
