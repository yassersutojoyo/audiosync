# AudioSync Google Cast sender specification

A behavioral specification for a Google Cast audio sender in AudioSync: a component that finds Cast devices on the local network, starts Google's Default Media Receiver on one of them, and has it play the host's live audio, fetched over HTTP from the host. It is written so the sender can be built by someone who reads only this document, the RFCs and format documents it cites, Google's public Cast documentation and the documentation of the crates they choose.

The sender drives one Cast device at a time. Cast groups and several independent Cast devices in one stream are noted as gaps (section 9). The sender never claims sample-accurate sync with local speakers or AirPlay receivers: section 7 says what it can and cannot control.

Provenance. Written on 2026-10-21 from public sources: Google's Cast developer documentation and support pages, community protocol write-ups, and the observed behavior of open-source Cast senders (pychromecast, go-chromecast, catt, mkchromecast and the node castv2 module), read as behavior and restated in our own words. No code, and no paraphrase of code, from any of them is in this document. Device authentication is described only as what a sender sees and whether a receiver requires it; nothing here describes how to forge or bypass it.

## Conventions

- MUST, MUST NOT, SHOULD, SHOULD NOT and MAY are normative, in the sense of RFC 2119.
- Each clause states one behavior of the sender. It ends with *Source:* (the kind of source the fact comes from; "derived" means computed or reasoned from facts in the cited sources rather than stated there) and, where AudioSync needs something the sources don't settle, *Required:* (the choice the sender MUST implement). "*Source:* AudioSync design" marks a clause that is a requirement only.
- A *Source:* of "observed behavior of open-source senders" reports what such a sender does, as read in its source or documentation. It is one implementation's reading of the protocol, not Google's, and several senders agreeing doesn't make a behavior Google's either.
- Latency figures marked "user reports" come from issue trackers. They are single observations on unnamed setups, quoted to show the order of magnitude and the spread, never as a specification.
- Terms: *sender* is the component this document specifies; *device* or *receiver* is the Cast device; *platform* is the device's built-in endpoint `receiver-0`; *app* is a receiver application running on the device, here the Default Media Receiver; *channel* is one TLS connection from the sender to one device; *session* is one channel plus the app it launched and the HTTP stream the app fetches, from connect to teardown; *host* is the AudioSync app that drives the sender; *stream server* is the HTTP server inside the sender that serves the audio.
- Message names and JSON field names are written as they appear on the wire, such as `GET_STATUS` and `transportId`; they are case-sensitive.
- Example addresses use 192.0.2.0/24 (RFC 5737).
- Clause IDs run from C1 without gaps in this version. A clause added later takes the next free number and sits in the section it belongs to; a dropped clause keeps its number, marked withdrawn.

## 1. Scope and role

- **C1** The sender handles audio only. It never loads video, images as the main media, or text tracks; artwork is sent only as part of track metadata (C52). *Source:* AudioSync design.
- **C2** A stream drives at most one Cast device in this version. Starting a stream on a second Cast device while one is playing is refused with a clear reason; the sender never runs two Cast sessions side by side. *Source:* AudioSync design; G1 and G2 track lifting the limit.
- **C3** Cast groups (several devices that Google software plays in step as one target) are out of scope for this version. They are found and shown as unsupported (C14), never driven. *Source:* AudioSync design; see G1.
- **C4** The sender uses only the Default Media Receiver app (C33). It never registers, hosts or launches a custom receiver app, and never needs a Google developer account. *Source:* AudioSync design; Google Cast Web Receiver overview [cast-receiver] (the Default Media Receiver needs no registration).
- **C5** Encrypted or DRM-protected media, and anything that needs a Custom Receiver, are out of scope. *Source:* Google Cast Supported Media [cast-media] (encrypted protocols require a Custom Receiver); AudioSync design.
- **C6** Device authentication (C25 to C27) is out of scope beyond what those clauses say. The sender never presents, forges, replays or works around Google-signed device credentials. *Source:* AudioSync design.
- **C7** A failure in the Cast session never stops the audio input, local speakers or AirPlay sessions of the same stream. *Source:* AudioSync design.
- **C8** The call through which the host hands audio to the sender MUST NOT allocate, lock, block, log or touch the file system. It copies frames into a preallocated single-producer, single-consumer ring and returns. Encoding, TLS and HTTP work run on the sender's own threads. *Source:* AudioSync design (the app's rule for audio callbacks).
- **C9** Every identifier the sender creates (request ids, stream URL tokens) is random or counter-based. The sender never sends a hardware address, serial number, host name or user name to the device. *Source:* AudioSync design.

## 2. Discovery

- **C10** The sender browses for `_googlecast._tcp` in the `local.` domain over multicast DNS, following PTR records to SRV, TXT and address records. *Source:* RFC 6762; RFC 6763; community protocol write-up [oakbits]; observed behavior of open-source senders [pychromecast].
- **C11** Instances under a subtype (names ending in `_sub._googlecast._tcp.local.`) are not devices and are ignored. *Source:* observed behavior of open-source senders [pychromecast]; RFC 6763 §7.1 (subtypes).
- **C12** Each TXT string is `key=value`; a string without `=` is skipped. Values are UTF-8 bytes as received. Tools that print DNS presentation format show non-ASCII bytes as a backslash and three decimal digits, and one open-source sender undoes that escaping in `fn` and `md` because its mDNS library leaves it in; the sender reads the raw record bytes, so the escaping never reaches it. Unknown keys are kept for diagnostics and otherwise ignored. *Source:* RFC 6763 §6; observed behavior of open-source senders [go-chromecast].
- **C13** TXT keys the sender reads:
  - `id`: the device's unique identifier, a UUID written as hex digits. An instance whose `id` is missing or does not parse as a UUID is skipped; one open-source sender does this to drop broken third-party emulators. The identifier is the device's key in the host's device list and settings.
  - `fn`: the friendly name the user set; it is the name shown in the host.
  - `md`: the model name, such as Chromecast or Chromecast Audio; shown as secondary text. A missing `md` is shown as an unknown model, never as an error.
  - `ve`: a protocol version identifier; the write-up lists 02 for the first Chromecast, 04 for Nexus Player and 05 for Chromecast Audio. Recorded for diagnostics only.
  - `ic`: a path on the device to its icon, such as `/setup/icon.png`. Not fetched by this version.
  - `ca`: sources disagree. One write-up calls it a certificate authority identifier (4100 on the first Chromecast, 5 on later devices); other readers treat it as a capability bit set. The sender MUST NOT gate any behavior on `ca`.
  - `rs`, `st`, `bs`, `cd`, `rm`, `nf`: seen in the wild; no source used here gives their meaning. Not interpreted.

  *Source:* community protocol write-up [oakbits]; observed behavior of open-source senders [pychromecast], [go-chromecast]. G3 tracks `ca` and the uninterpreted keys.
- **C14** An instance whose SRV port is not 8009 is a Cast group, not a single device. It is listed as a group and marked unsupported (C3). *Source:* observed behavior of open-source senders [pychromecast] (a port other than 8009 marks a group). G1 settles whether the port alone is enough.
- **C15** The sender connects to the address and port from the SRV and address records, never to a port it assumes. Single devices announce port 8009. *Source:* community protocol write-up [oakbits] (port 8009, announced in DNS-SD); RFC 6763 §5.
- **C16** A device seen on several interfaces, or with both IPv4 and IPv6 addresses, is one device keyed by `id`. *Source:* AudioSync design. *Required:* the sender tries IPv4 addresses first, then IPv6, and remembers the address that worked.
- **C17** A device whose records expire (goodbye packet or TTL) is marked offline but kept in the list while a session with it is open; the session's own health (C30) decides whether it ends. *Source:* RFC 6762 §10.1 (goodbye packets); AudioSync design.
- **C18** The host MAY let the user add a device by address when multicast DNS is blocked. Such a device has no TXT data; it is keyed by address until a later browse finds its `id`. *Source:* observed behavior of open-source senders [pychromecast] (known addresses can be passed when discovery can't work; multicast DNS needs UDP port 5353 open); AudioSync design.
