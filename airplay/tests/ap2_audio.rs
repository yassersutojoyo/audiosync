//! The AirPlay 2 audio path and its timing (sections 4 and 6 of
//! docs/AIRPLAY_SPEC.md): sealed realtime packets (P155, P156), the PTP
//! service (P158 to P163, P167) and time-announce packets (P166), first on
//! the stream and the service alone on a fake clock, then through the
//! sender's driver to a fake receiver on loopback sockets that opens every
//! packet and reads every PTP message with its own code.

mod common;

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use audiosync_airplay::ap2::{StreamKey, Timing};
use audiosync_airplay::audio::Frame;
use audiosync_airplay::clock::{NTP_ORIGIN_SECONDS, SenderClock};
use audiosync_airplay::discovery::{
    Device, DeviceId, DeviceKey, ScopedAddr, ServiceKind, ServiceRecord,
};
use audiosync_airplay::error::{ErrorKind, UnsupportedReason, Warning};
use audiosync_airplay::ptp::{PtpPorts, PtpService, PtpTransmit, clock_identity};
use audiosync_airplay::rtp::{RESEND_PREFIX_BYTES, ResendRequest};
use audiosync_airplay::sdp::Codec;
use audiosync_airplay::sender::{
    Sender, SenderConfig, SpeakerEvent, SpeakerOptions, SpeakerState, TimingMemory,
};
use audiosync_airplay::stream::{Ap2Audio, PacketKind, SpeakerConfig, StreamEvent};
use audiosync_airplay::targets::{Member, Target, TargetId, TargetKind};
use audiosync_airplay::volume::Volume;

use common::ap2::{
    Ap2Server, FakeAp2, dict, int, open_packet, plist, read_dict, request_key, stream_key,
};
use common::net::{
    Events, HostAudio, free_ptp_ports, host_frame_of, ptp_bind_v4, serial, wait_until,
};
use common::rig::{Rig, decode, speaker};

const LONG: Duration = Duration::from_secs(15);
const IDENTITY: [u8; 8] = [0xFF, 0xFF, 0x0A, 0x0B, 0x0C, 0x0D, 0x0E, 0x0F];

fn ap2_speaker(k: u8, key: [u8; 32], clock_identity: Option<[u8; 8]>) -> SpeakerConfig {
    let mut config = speaker(k, Codec::Alac);
    config.airplay2 = Some(Ap2Audio {
        key: StreamKey::new(key),
        clock_identity,
    });
    config
}

fn seq_of(bytes: &[u8]) -> u16 {
    u16::from_be_bytes([bytes[2], bytes[3]])
}

/// Whether RTP `seq` comes before `next` in sequence order.
fn before(seq: u16, next: u16) -> bool {
    (next.wrapping_sub(seq) as i16) > 0
}

/// The most packets a pause may let out after the call: 100 ms of 352-frame
/// packets, a count so a late arrival stamp can't inflate it (P213).
const LATE_PACKETS: usize = 13;

/// The seqs of the datagrams that arrived after `at` and come before `held`,
/// asserted to be the run that ends right before it, each packet once.
fn tail_before(datagrams: &[Datagram], at: Instant, held: u16) -> Vec<u16> {
    let tail: Vec<u16> = datagrams
        .iter()
        .filter(|(arrived, _, bytes)| *arrived > at && before(seq_of(bytes), held))
        .map(|(_, _, bytes)| seq_of(bytes))
        .collect();
    assert!(
        tail.windows(2)
            .all(|pair| pair[1] == pair[0].wrapping_add(1)),
        "{tail:?}"
    );
    assert!(
        tail.last().is_none_or(|last| last.wrapping_add(1) == held),
        "{tail:?} before {held}"
    );
    assert!(tail.len() <= LATE_PACKETS, "{tail:?}");
    tail
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

// covers: P155, P156
#[test]
fn ap2_audio_packet_seal() {
    let key = [0xA7; 32];
    let mut rig = Rig::with_host();
    let sealed = rig.add(ap2_speaker(1, key, None));
    let plain = rig.add(speaker(2, Codec::Alac));
    rig.run_for(Duration::from_secs(3));

    let packets: Vec<Vec<u8>> = rig
        .of(sealed)
        .into_iter()
        .filter(|s| s.transmit.kind == PacketKind::Audio)
        .map(|s| s.transmit.bytes.clone())
        .collect();
    assert!(packets.len() > 300);
    let reference: HashMap<i64, Vec<u8>> = rig
        .audio(plain)
        .into_iter()
        .map(|a| (rig.frame_of(plain, a.header.timestamp), a.payload))
        .collect();
    for (index, bytes) in packets.iter().enumerate() {
        // Header in the clear, then ciphertext, tag and the counter.
        let opened = open_packet(&key, bytes).expect("the packet opens with shk");
        assert_eq!(bytes.len(), opened.payload.len() + 36);
        assert_eq!(bytes[0], 0x80);
        assert_eq!(bytes[1], if index == 0 { 0xE0 } else { 0x60 });
        assert_eq!(u32_at(bytes, 8), 0x5EED_1001);
        // The counter: 0 for the first packet under the key, then one more
        // for each, little-endian in the trailer.
        assert_eq!(opened.counter, index as u64);
        assert_eq!(
            &bytes[bytes.len() - 8..],
            &(index as u64).to_le_bytes(),
            "little-endian"
        );
        let n = rig.frame_of(sealed, opened.timestamp());
        assert_eq!(opened.payload, reference[&n], "the shared payload");
        assert_ne!(&bytes[12..12 + opened.payload.len()], &opened.payload[..]);
        assert_eq!(decode(Codec::Alac, &opened.payload).len(), 352);
    }
    // The additional data is header bytes 4 to 11; bytes 0 to 3 are not in
    // it, and the tag guards the rest.
    let sample = &packets[5];
    for at in [4, 7, 8, 11] {
        let mut bad = sample.clone();
        bad[at] ^= 1;
        assert!(open_packet(&key, &bad).is_none(), "byte {at} is guarded");
    }
    let mut unguarded = sample.clone();
    unguarded[3] ^= 1;
    assert!(
        open_packet(&key, &unguarded).is_some(),
        "seq is not in the AAD"
    );
    let mut bad = sample.clone();
    let tag_at = bad.len() - 24;
    bad[tag_at] ^= 1;
    assert!(open_packet(&key, &bad).is_none());
    assert!(open_packet(&[0xA8; 32], sample).is_none());

    // A resend is the sealed packet exactly as first sent (P87, P155).
    let last = seq_of(packets.last().expect("packet"));
    let first = last.wrapping_sub(2);
    let request = ResendRequest {
        seq: 1,
        first,
        count: 3,
    }
    .encode();
    let before = rig.sent.len();
    rig.stream.handle_control_packet(sealed, &request, rig.now);
    rig.drain();
    let replies: Vec<Vec<u8>> = rig.sent[before..]
        .iter()
        .filter(|s| s.transmit.kind == PacketKind::Resend)
        .map(|s| s.transmit.bytes.clone())
        .collect();
    assert_eq!(replies.len(), 3);
    for (offset, reply) in replies.iter().enumerate() {
        let seq = first.wrapping_add(offset as u16);
        assert_eq!(
            &reply[..RESEND_PREFIX_BYTES],
            &[0x80, 0xD6, (seq >> 8) as u8, seq as u8]
        );
        let original = packets.iter().find(|p| seq_of(p) == seq).expect("sent");
        assert_eq!(&reply[RESEND_PREFIX_BYTES..], &original[..]);
    }

    // A FLUSH keeps the key and the counter runs on (P156).
    rig.stream
        .set_delay(sealed, Duration::from_millis(200), rig.now);
    rig.run_for(Duration::from_secs(2));
    assert!(
        rig.events
            .iter()
            .any(|event| matches!(event, StreamEvent::Flush { speaker, .. } if *speaker == sealed))
    );
    let all: Vec<Vec<u8>> = rig
        .of(sealed)
        .into_iter()
        .filter(|s| s.transmit.kind == PacketKind::Audio)
        .map(|s| s.transmit.bytes.clone())
        .collect();
    assert!(all.len() > packets.len() + 100);
    let mut markers = 0;
    for (index, bytes) in all.iter().enumerate() {
        let opened = open_packet(&key, bytes).expect("the same key after FLUSH");
        assert_eq!(opened.counter, index as u64, "never repeats");
        markers += usize::from(bytes[1] == 0xE0);
    }
    assert_eq!(markers, 2, "RECORD and the FLUSH");

    // A new stream SETUP brings a new key, and its counter starts at 0.
    let second_key = [0x3C; 32];
    let second = rig.add(ap2_speaker(3, second_key, None));
    rig.run_for(Duration::from_secs(1));
    let fresh: Vec<Vec<u8>> = rig
        .of(second)
        .into_iter()
        .filter(|s| s.transmit.kind == PacketKind::Audio)
        .map(|s| s.transmit.bytes.clone())
        .collect();
    assert!(fresh.len() > 50);
    for (index, bytes) in fresh.iter().enumerate() {
        assert_eq!(
            open_packet(&second_key, bytes).expect("opens").counter,
            index as u64
        );
        assert!(open_packet(&key, bytes).is_none());
    }

    // The key never shows in diagnostics.
    let debug = format!("{:?}", ap2_speaker(1, key, None));
    assert!(!debug.contains("167, 167"), "{debug}");
    assert!(!format!("{:?}", StreamKey::new(key)).contains("167"));
}

/// The fields of a PTP header (IEEE 1588-2008 §13.3), read on their own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Header {
    transport: u8,
    kind: u8,
    version: u8,
    length: u16,
    domain: u8,
    flags: u16,
    correction: [u8; 8],
    identity: [u8; 8],
    port: u16,
    seq: u16,
    control: u8,
    log_interval: i8,
}

fn header(bytes: &[u8]) -> Header {
    assert!(bytes.len() >= 34);
    let mut correction = [0u8; 8];
    correction.copy_from_slice(&bytes[8..16]);
    let mut identity = [0u8; 8];
    identity.copy_from_slice(&bytes[20..28]);
    Header {
        transport: bytes[0] >> 4,
        kind: bytes[0] & 0x0F,
        version: bytes[1] & 0x0F,
        length: u16::from_be_bytes([bytes[2], bytes[3]]),
        domain: bytes[4],
        flags: u16::from_be_bytes([bytes[6], bytes[7]]),
        correction,
        identity,
        port: u16::from_be_bytes([bytes[28], bytes[29]]),
        seq: u16::from_be_bytes([bytes[30], bytes[31]]),
        control: bytes[32],
        log_interval: bytes[33] as i8,
    }
}

/// A 10-byte PTP timestamp: 48-bit seconds, 32-bit nanoseconds.
fn timestamp(bytes: &[u8]) -> (u64, u32) {
    let mut seconds = [0u8; 8];
    seconds[2..].copy_from_slice(&bytes[..6]);
    (u64::from_be_bytes(seconds), u32_at(bytes, 6))
}

/// A message to the service from `identity`, of type `kind`.
fn message(kind: u8, identity: [u8; 8], seq: u16, length: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; length];
    bytes[0] = 0x10 | kind;
    bytes[1] = 2;
    bytes[2..4].copy_from_slice(&(length as u16).to_be_bytes());
    bytes[20..28].copy_from_slice(&identity);
    bytes[28..30].copy_from_slice(&7u16.to_be_bytes());
    bytes[30..32].copy_from_slice(&seq.to_be_bytes());
    bytes
}

fn drain(service: &mut PtpService) -> Vec<PtpTransmit> {
    std::iter::from_fn(|| service.poll_transmit()).collect()
}

// covers: P17, P162
#[test]
fn ptp_keeps_the_scope_of_a_link_local_receiver() {
    let clock = SenderClock::new(Instant::now());
    let ports = PtpPorts {
        event: 31_900,
        general: 32_000,
    };
    let ip = Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0x30);
    let mut service = PtpService::new(clock, IDENTITY, ports);
    let now = Instant::now();
    service.start(SocketAddr::V6(SocketAddrV6::new(ip, 7000, 0, 7)), now);
    let sent = drain(&mut service);
    assert!(!sent.is_empty());
    for transmit in &sent {
        let SocketAddr::V6(to) = transmit.to else {
            panic!("{:?}", transmit.to);
        };
        assert_eq!((*to.ip(), to.scope_id()), (ip, 7), "{to}");
    }
    let theirs = [0x00, 0x11, 0x22, 0xFF, 0xFE, 0x33, 0x44, 0x55];
    let source = SocketAddr::V6(SocketAddrV6::new(ip, 41_234, 0, 7));
    service.handle_message(&message(0x1, theirs, 3, 44), source, now);
    let answers = drain(&mut service);
    assert_eq!(answers.len(), 1);
    assert_eq!(
        answers[0].to,
        SocketAddr::V6(SocketAddrV6::new(ip, ports.general, 0, 7))
    );
}

// covers: P159, P160, P161, P162, P163, P167
#[test]
fn ptp_messages() {
    let origin = Instant::now();
    let clock = SenderClock::new(origin);
    let ports = PtpPorts {
        event: 31_900,
        general: 32_000,
    };
    let peer = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 30));
    let mut service = PtpService::new(clock, IDENTITY, ports);
    let t0 = origin + Duration::from_millis(5_000_250);

    // With no PTP session it sends nothing (P167).
    service.handle_timeout(t0);
    assert!(service.poll_timeout().is_none());
    assert!(drain(&mut service).is_empty());

    // From the session SETUP answer: an Announce at once, a Sync every
    // 125 ms, and a Follow_Up after each Sync (P160, P161, P167).
    service.start(SocketAddr::new(peer, 0), t0);
    let mut sent: Vec<(Instant, PtpTransmit)> = Vec::new();
    let mut follow_up_times: HashMap<u16, Instant> = HashMap::new();
    let mut now = t0;
    while now < t0 + Duration::from_millis(3_000) {
        service.handle_timeout(now);
        for transmit in drain(&mut service) {
            if header(&transmit.bytes).kind == 0x0 {
                let at = now + Duration::from_micros(40);
                follow_up_times.insert(transmit.sequence_id, at);
                service.sync_sent(transmit.sequence_id, at);
            }
            sent.push((now, transmit));
        }
        for transmit in drain(&mut service) {
            sent.push((now, transmit));
        }
        now += Duration::from_millis(5);
    }
    let of = |kind: u8| -> Vec<&(Instant, PtpTransmit)> {
        sent.iter()
            .filter(|(_, t)| header(&t.bytes).kind == kind)
            .collect()
    };
    let announces = of(0xB);
    let syncs = of(0x0);
    let follow_ups = of(0x8);
    assert_eq!(sent[0].1.bytes[0] & 0x0F, 0xB, "the first Announce at once");
    assert_eq!(sent[0].0, t0);
    assert_eq!(announces.len(), 3);
    assert_eq!(syncs.len(), 24);
    assert_eq!(follow_ups.len(), 24);
    for pair in announces.windows(2) {
        assert_eq!(pair[1].0 - pair[0].0, Duration::from_secs(1));
    }
    for pair in syncs.windows(2) {
        assert_eq!(pair[1].0 - pair[0].0, Duration::from_millis(125));
    }

    // Every message: version 2, transportSpecific 1, domain 0, correction
    // 0, the clock identity and port 1 (P159); unicast to the receiver.
    for (_, transmit) in &sent {
        let h = header(&transmit.bytes);
        assert_eq!(h.transport, 1);
        assert_eq!(h.version, 2);
        assert_eq!(usize::from(h.length), transmit.bytes.len());
        assert_eq!(h.domain, 0);
        assert_eq!(h.correction, [0; 8]);
        assert_eq!(h.identity, IDENTITY);
        assert_eq!(h.port, 1);
        assert_eq!(transmit.to.ip(), peer);
        let (flags, port) = match h.kind {
            0x0 => (0x0608, ports.event),
            _ => (0x0408, ports.general),
        };
        assert_eq!(h.flags, flags, "{h:?}");
        assert_eq!(transmit.to.port(), port);
    }
    // Each type counts its own sequenceId from 0.
    for (index, (_, a)) in announces.iter().enumerate() {
        assert_eq!(header(&a.bytes).seq, index as u16);
    }
    for (index, (_, s)) in syncs.iter().enumerate() {
        assert_eq!(header(&s.bytes).seq, index as u16);
    }
    assert_eq!(
        clock_identity([1, 2, 3, 4, 5, 6]),
        [0xFF, 0xFF, 1, 2, 3, 4, 5, 6]
    );

    // Announce (P160).
    let a = &announces[0].1.bytes;
    let h = header(a);
    assert_eq!((h.control, h.log_interval), (5, 0));
    assert_eq!(timestamp(&a[34..44]), (0, 0), "originTimestamp 0");
    assert_eq!(&a[44..46], &[0, 0], "currentUtcOffset 0");
    assert_eq!(a[47], 128, "priority1");
    assert_eq!(&a[48..52], &[6, 0x21, 0x43, 0x6A], "clock quality");
    assert_eq!(a[52], 128, "priority2");
    assert_eq!(&a[53..61], &IDENTITY, "grandmasterIdentity");
    assert_eq!(&a[61..63], &[0, 0], "stepsRemoved");
    assert_eq!(a[63], 0x20, "timeSource");
    assert_eq!(&a[64..66], &[0x00, 0x08], "path trace TLV");
    assert_eq!(&a[66..68], &[0, 8]);
    assert_eq!(&a[68..76], &IDENTITY);
    assert_eq!(a.len(), 76);

    // Sync and Follow_Up (P161, P163).
    for ((_, sync), (_, follow)) in syncs.iter().zip(&follow_ups) {
        let s = &sync.bytes;
        let hs = header(s);
        assert_eq!((hs.control, hs.log_interval), (0, -3));
        assert_eq!(s.len(), 44);
        assert_eq!(timestamp(&s[34..44]), (0, 0));
        let f = &follow.bytes;
        let hf = header(f);
        assert_eq!(hf.seq, hs.seq, "the Sync's sequenceId");
        assert_eq!(hf.control, 2);
        let (seconds, nanos) = timestamp(&f[34..44]);
        let sent_at = follow_up_times[&hs.seq];
        let since = sent_at - origin;
        assert_eq!((seconds, nanos), (since.as_secs(), since.subsec_nanos()));
        // The PTP seconds are the NTP-format seconds minus 0x83AA7E80.
        let ntp = clock.ntp(sent_at);
        assert_eq!(u64::from(ntp.seconds()) - NTP_ORIGIN_SECONDS, seconds);
        assert_eq!(&f[44..48], &[0, 3, 0, 28], "802.1AS TLV");
        assert_eq!(&f[48..54], &[0x00, 0x80, 0xC2, 0x00, 0x00, 0x01]);
        assert!(f[54..76].iter().all(|&b| b == 0));
        assert_eq!(&f[76..80], &[0, 3, 0, 14], "Apple TLV");
        assert_eq!(&f[80..86], &[0x00, 0x0D, 0x93, 0x00, 0x00, 0x04]);
        assert_eq!(&f[86..94], &IDENTITY);
        assert_eq!(f.len(), 94);
    }

    // A Delay_Req from the receiver's address, any port, gets a Delay_Resp
    // to port 320 of that address (P162).
    let theirs = [0x00, 0x11, 0x22, 0xFF, 0xFE, 0x33, 0x44, 0x55];
    let received = now + Duration::from_millis(1);
    let source = SocketAddr::new(peer, 41_234);
    service.handle_message(&message(0x1, theirs, 0x0102, 44), source, received);
    let answers = drain(&mut service);
    assert_eq!(answers.len(), 1);
    let r = &answers[0].bytes;
    let hr = header(r);
    assert_eq!(hr.kind, 0x9);
    assert_eq!(hr.seq, 0x0102);
    assert_eq!(hr.flags, 0x0608);
    assert_eq!((hr.control, hr.log_interval), (3, -3));
    assert_eq!(answers[0].to, SocketAddr::new(peer, ports.general));
    let since = received - origin;
    assert_eq!(
        timestamp(&r[34..44]),
        (since.as_secs(), since.subsec_nanos())
    );
    assert_eq!(&r[44..52], &theirs);
    assert_eq!(&r[52..54], &[0, 7], "requestingPortIdentity");
    assert_eq!(r.len(), 54);

    // From another address, and every other message, nothing (P162); the
    // sender never follows another clock (P158).
    let next = service.poll_timeout();
    let stranger = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 31)), 319);
    service.handle_message(&message(0x1, theirs, 9, 44), stranger, received);
    for (kind, length) in [
        (0x2, 54),
        (0xC, 44),
        (0xD, 48),
        (0x0, 44),
        (0x8, 44),
        (0xB, 64),
    ] {
        service.handle_message(&message(kind, theirs, 9, length), source, received);
    }
    assert!(drain(&mut service).is_empty());
    assert_eq!(service.poll_timeout(), next);

    // The session ends: nothing more (P167).
    service.stop();
    service.handle_timeout(now + Duration::from_secs(5));
    service.handle_message(&message(0x1, theirs, 3, 44), source, received);
    assert!(drain(&mut service).is_empty());
    assert!(service.poll_timeout().is_none());
}

// covers: P166
#[test]
fn ptp_time_announce() {
    let mut rig = Rig::with_host();
    let ptp = rig.add(ap2_speaker(1, [0x11; 32], Some(IDENTITY)));
    let ntp = rig.add(ap2_speaker(2, [0x22; 32], None));
    rig.run_for(Duration::from_secs(3));
    rig.stream
        .set_delay(ptp, Duration::from_millis(150), rig.now);
    rig.run_for(Duration::from_secs(2));

    // Under NTP the sync packets stay (P157).
    let ntp_syncs: Vec<_> = rig
        .of(ntp)
        .into_iter()
        .filter(|s| s.transmit.kind == PacketKind::Sync)
        .collect();
    assert!(ntp_syncs.iter().all(|s| s.transmit.bytes.len() == 20));

    let sent = rig.of(ptp);
    let mut announces = Vec::new();
    for (index, s) in sent.iter().enumerate() {
        if s.transmit.kind != PacketKind::Sync {
            continue;
        }
        // Each goes right before the packet whose timestamp it names.
        let next = sent[index + 1..]
            .iter()
            .find(|n| n.transmit.kind == PacketKind::Audio)
            .expect("the next packet");
        announces.push((s.transmit.bytes.clone(), next.transmit.bytes.clone()));
    }
    assert!(announces.len() >= 5);
    let latency = rig.stream.latency_frames();
    let mut firsts = 0;
    for (index, (bytes, next)) in announces.iter().enumerate() {
        assert_eq!(bytes.len(), 28);
        assert_eq!(bytes[1], 0xD7);
        let first = bytes[0] == 0x90;
        assert!(first || bytes[0] == 0x80);
        firsts += usize::from(first);
        assert_eq!(seq_of(bytes), index as u16, "counts up from 0");
        let next_rtp = u32_at(next, 4);
        assert_eq!(u32_at(bytes, 16), next_rtp.wrapping_sub(11_025));
        assert_eq!(u32_at(bytes, 4), next_rtp.wrapping_sub(latency));
        assert_eq!(&bytes[20..28], &IDENTITY);
    }
    assert_eq!(firsts, 2, "after RECORD and after the FLUSH");
    assert_eq!(announces[0].0[0], 0x90);
    // Bytes 8 to 15: the nanoseconds of PTP time (P163) at which the frame
    // of bytes 4 to 7 is heard, P_s(n) = c(n) + L + d_s (P97).
    let flush_at = announces
        .iter()
        .rposition(|(bytes, _)| bytes[0] == 0x90)
        .expect("the flush");
    for (bytes, _) in &announces[flush_at..] {
        let n = rig.frame_of(ptp, u32_at(bytes, 4));
        let heard = rig.c(n) + rig.latency() + 0.150;
        let mut nanos = [0u8; 8];
        nanos.copy_from_slice(&bytes[8..16]);
        let stated = u64::from_be_bytes(nanos) as f64 / 1e9;
        assert!((stated - heard).abs() < 1e-6, "{stated} {heard}");
    }
}

type Datagram = (Instant, SocketAddr, Vec<u8>);

/// A loopback socket of the fake receiver, read on its own thread from the
/// moment it is bound, so no datagram is lost to a full receive buffer
/// while the test is busy elsewhere.
struct Recorder {
    socket: UdpSocket,
    got: Arc<Mutex<Vec<Datagram>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Recorder {
    fn bind() -> Recorder {
        let socket = UdpSocket::bind("127.0.0.1:0").expect("a socket");
        let reader = socket.try_clone().expect("a clone");
        let got = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let got = Arc::clone(&got);
            let stop = Arc::clone(&stop);
            // No read timeout: on Windows a receive that times out can drop
            // the datagram arriving at that moment. `drop` wakes the reader.
            std::thread::spawn(move || {
                let mut buffer = [0u8; 4096];
                loop {
                    let received = reader.recv_from(&mut buffer);
                    if stop.load(Ordering::Acquire) {
                        break;
                    }
                    if let Ok((len, from)) = received {
                        let datagram = (Instant::now(), from, buffer[..len].to_vec());
                        got.lock().expect("datagrams").push(datagram);
                    }
                }
            })
        };
        Recorder {
            socket,
            got,
            stop,
            thread: Some(thread),
        }
    }

    fn port(&self) -> u16 {
        self.socket.local_addr().expect("address").port()
    }

    /// Everything received since the last `take`.
    fn take(&self) -> Vec<Datagram> {
        std::mem::take(&mut *self.got.lock().expect("datagrams"))
    }

    /// Everything received since the last `take` and within `span` more.
    fn after(&self, span: Duration) -> Vec<Datagram> {
        std::thread::sleep(span);
        self.take()
    }

    /// Only what arrives within `span` from now.
    fn fresh(&self, span: Duration) -> Vec<Datagram> {
        self.take();
        self.after(span)
    }

    /// The first datagram since the last `take` that matches, waiting up
    /// to [`LONG`] for it; what was received stays for the next `take`.
    fn wait_for(&self, matches: impl Fn(&Datagram) -> bool) -> Option<Datagram> {
        let mut found = None;
        wait_until(LONG, || {
            found = self
                .got
                .lock()
                .expect("datagrams")
                .iter()
                .find(|datagram| matches(datagram))
                .cloned();
            found.is_some()
        });
        found
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        // The reader blocks without a timeout, so join only once the wake went out.
        let woken = UdpSocket::bind("127.0.0.1:0")
            .and_then(|waker| waker.send_to(&[], ("127.0.0.1", self.port())))
            .is_ok();
        if let Some(thread) = self.thread.take().filter(|_| woken) {
            let _ = thread.join();
        }
    }
}

/// A fake receiver's UDP side: the stream's data and control sockets and
/// its PTP event and general sockets, all on loopback.
struct Udp {
    data: Recorder,
    control: Recorder,
    event: Recorder,
    general: Recorder,
}

impl Udp {
    fn bind() -> Udp {
        Udp {
            data: Recorder::bind(),
            control: Recorder::bind(),
            event: Recorder::bind(),
            general: Recorder::bind(),
        }
    }

    fn ptp_ports(&self) -> PtpPorts {
        PtpPorts {
            event: self.event.port(),
            general: self.general.port(),
        }
    }

    fn fake(&self, mut fake: FakeAp2) -> FakeAp2 {
        fake.data_port = self.data.port();
        fake.control_port = self.control.port();
        fake
    }
}

/// Whether someone else could bind both of `ports` now.
fn ports_free(ports: PtpPorts) -> bool {
    UdpSocket::bind((ptp_bind_v4(), ports.event)).is_ok()
        && UdpSocket::bind((ptp_bind_v4(), ports.general)).is_ok()
}

fn device(server: &Ap2Server, id: u8, features: &str) -> Device {
    let device_id = DeviceId([0x02, 0x00, 0x5E, 0x00, 0x53, id]);
    let record = ServiceRecord {
        kind: ServiceKind::AirPlay,
        instance: format!("Speaker {id}"),
        host: format!("speaker-{id}.local"),
        port: server.address.port(),
        txt: [
            format!("features={features}"),
            "flags=0x4".to_owned(),
            "model=Speaker1,1".to_owned(),
            format!("deviceid={device_id}"),
        ]
        .into_iter()
        .map(String::into_bytes)
        .collect(),
        addresses: vec![ScopedAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST))],
    };
    Device::from_records(DeviceKey::Id(device_id), None, Some(&record)).expect("device")
}

fn start(clock: SenderClock, ptp_ports: PtpPorts, receiver: PtpPorts) -> (Sender, HostAudio) {
    let config = SenderConfig {
        airplay2: true,
        clock: Some(clock),
        ptp_ports,
        ptp_receiver_ports: receiver,
        ..SenderConfig::default()
    };
    let (sender, writer) = Sender::start(config).expect("the sender starts");
    (sender, HostAudio::start(writer))
}

fn options() -> SpeakerOptions {
    SpeakerOptions::new(Volume::Db(-20.0))
}

fn playing(events: &mut Events, sender: &Sender, key: &DeviceKey, nth: usize) -> bool {
    events
        .wait_state(sender, key, nth, LONG, |s| *s == SpeakerState::Playing)
        .is_some()
}

fn rtp_info(server: &Ap2Server) -> (u16, u32) {
    let flush = server.fake().last("FLUSH").cloned().expect("FLUSH");
    let info = flush.headers.get("RTP-Info").expect("RTP-Info").to_owned();
    let mut seq = 0;
    let mut rtptime = 0;
    for part in info.split(';') {
        if let Some(value) = part.strip_prefix("seq=") {
            seq = value.parse().expect("seq");
        }
        if let Some(value) = part.strip_prefix("rtptime=") {
            rtptime = value.parse().expect("rtptime");
        }
    }
    (seq, rtptime)
}

// covers: P155, P156
#[test]
fn ap2_audio_reaches_the_receiver() {
    let _serial = serial();
    let udp = Udp::bind();
    let server = Ap2Server::start(udp.fake(FakeAp2::new()));
    let clock = SenderClock::new(Instant::now());
    let (sender, _host) = start(clock, free_ptp_ports(), udp.ptp_ports());
    let mut events = Events::default();
    let speaker = device(&server, 61, "0x0,0x10040");
    let key = speaker.key.clone();
    sender.add_speaker(speaker.clone(), options());
    assert!(
        playing(&mut events, &sender, &key, 1),
        "{:?} {:?}",
        events.of(&key),
        server.fake().keys()
    );

    let audio = udp.data.after(Duration::from_millis(1_500));
    let shk = stream_key(&server.fake()).expect("shk");
    assert_ne!(shk, [0; 32]);
    assert!(audio.len() > 100, "{}", audio.len());
    let (first_seq, first_rtptime) = rtp_info(&server);
    let mut heard = 0;
    for (index, (_, _, bytes)) in audio.iter().enumerate() {
        // The receiver opens every packet with the `shk` of the SETUP.
        let opened = open_packet(&shk, bytes).expect("the packet opens");
        assert_eq!(opened.counter, index as u64);
        assert_eq!(opened.seq(), first_seq.wrapping_add(index as u16));
        assert_eq!(
            opened.timestamp(),
            first_rtptime.wrapping_add(352 * index as u32)
        );
        assert_eq!(bytes[1], if index == 0 { 0xE0 } else { 0x60 });
        let frames = decode(Codec::Alac, &opened.payload);
        assert_eq!(frames.len(), 352);
        heard += frames
            .iter()
            .filter(|f| host_frame_of(**f).is_some())
            .count();
    }
    assert!(heard > 0, "the host's audio is in the packets");

    // Sync packets under NTP come from the control socket, which takes
    // retransmission requests; a resend is the packet as first sent.
    udp.control.take();
    let (_, sender_control, sync) = udp
        .control
        .wait_for(|(_, _, bytes)| bytes.len() == 20 && bytes[1] == 0xD4)
        .expect("a sync packet");
    assert_eq!(sync.len(), 20);
    let wanted = seq_of(&audio[3].2);
    let request = ResendRequest {
        seq: 1,
        first: wanted,
        count: 1,
    }
    .encode();
    udp.control.take();
    udp.control
        .socket
        .send_to(&request, sender_control)
        .expect("a resend request");
    let reply = udp
        .control
        .wait_for(|(_, _, bytes)| bytes.len() > 4 && bytes[1] == 0xD6)
        .expect("a resend");
    assert_eq!(&reply.2[RESEND_PREFIX_BYTES..], &audio[3].2[..]);

    // A new session: a new stream SETUP, a new key, a counter from 0.
    sender.remove_speaker(&key);
    assert!(wait_until(LONG, || server
        .fake()
        .keys()
        .iter()
        .filter(|k| *k == "TEARDOWN")
        .count()
        == 2));
    events.drain(&sender);
    let _ = udp.data.fresh(Duration::from_millis(300));
    sender.add_speaker(speaker, options());
    assert!(
        playing(&mut events, &sender, &key, 2),
        "{:?}",
        events.of(&key)
    );
    let (_, _, again) = udp.data.wait_for(|_| true).expect("audio again");
    let second = stream_key(&server.fake()).expect("shk");
    assert_ne!(second, shk);
    assert_eq!(open_packet(&second, &again).expect("opens").counter, 0);
    assert!(open_packet(&shk, &again).is_none());
    sender.shutdown();
}

/// Each frame a member's sealed ALAC packets carry, by the host frame it
/// came from, asserted to run on by one within every packet.
fn member_frames(key: &[u8; 32], datagrams: &[Datagram]) -> BTreeMap<u64, Frame> {
    let mut frames = BTreeMap::new();
    for (_, _, bytes) in datagrams {
        let opened = open_packet(key, bytes).expect("the member's own key opens it");
        let decoded = decode(Codec::Alac, &opened.payload);
        let numbered: Vec<(u64, Frame)> = decoded
            .iter()
            .filter_map(|frame| Some((host_frame_of(*frame)?, *frame)))
            .collect();
        assert!(
            numbered.windows(2).all(|pair| pair[1].0 == pair[0].0 + 1),
            "a frame that is not the host's stereo frame"
        );
        frames.extend(numbered);
    }
    frames
}

// covers: P178
#[test]
fn ap2_a_pair_sends_every_member_the_same_full_stereo_audio() {
    let _serial = serial();
    let (udp_a, udp_b) = (Udp::bind(), Udp::bind());
    let a = Ap2Server::start(udp_a.fake(FakeAp2::new()));
    let b = Ap2Server::start(udp_b.fake(FakeAp2::new()));
    let clock = SenderClock::new(Instant::now());
    let (sender, _host) = start(clock, free_ptp_ports(), udp_a.ptp_ports());
    let mut events = Events::default();
    let (da, db) = (device(&a, 71, "0x0,0x10040"), device(&b, 72, "0x0,0x10040"));
    let member = |device: &Device, name: &str| Member {
        key: device.key.clone(),
        name: name.to_owned(),
        device: Some(Box::new(device.clone())),
    };
    let target = Target {
        id: TargetId {
            kind: TargetKind::StereoPair,
            key: "a1b2c3d4".to_owned(),
        },
        name: "Kitchen".to_owned(),
        members: vec![member(&da, "Left"), member(&db, "Right")],
        leader: Some(da.key.clone()),
        control: None,
        expected_members: Some(2),
        partner_unreachable: false,
    };
    sender.add_target(target, options(), |_, _| {});
    for key in [&da.key, &db.key] {
        assert!(
            playing(&mut events, &sender, key, 1),
            "{:?}",
            events.of(key)
        );
    }

    let heard_a = udp_a.data.after(Duration::from_millis(1_500));
    let heard_b = udp_b.data.take();
    let shk_a = stream_key(&a.fake()).expect("shk");
    let shk_b = stream_key(&b.fake()).expect("shk");
    assert_ne!(shk_a, shk_b, "every member keeps its own key");
    let (frames_a, frames_b) = (
        member_frames(&shk_a, &heard_a),
        member_frames(&shk_b, &heard_b),
    );
    let mut shared = 0;
    for (frame, samples) in &frames_a {
        if let Some(other) = frames_b.get(frame) {
            assert_eq!(samples, other, "host frame {frame}");
            shared += 1;
        }
    }
    assert!(shared > 10_000, "{shared}");
    sender.shutdown();
}

// Part of P213 on the AirPlay 2 path: the pause sends the realtime FLUSH,
// whose answer releases the resume while the answer to a delay FLUSH still
// in flight does not, and POST /feedback goes on while paused.
#[test]
fn ap2_pause_flushes_and_the_resume_waits_for_its_answer() {
    const ANSWER: Duration = Duration::from_millis(800);
    // Feedback answered this late goes out every third interval, as on a
    // runner too loaded to keep the 2 s rhythm.
    const SLOW_FEEDBACK: Duration = Duration::from_millis(4_200);
    let _serial = serial();
    let udp = Udp::bind();
    let server = Ap2Server::start(udp.fake(FakeAp2::new()));
    let clock = SenderClock::new(Instant::now());
    let (sender, _host) = start(clock, free_ptp_ports(), udp.ptp_ports());
    let mut events = Events::default();
    let speaker = device(&server, 62, "0x0,0x10040");
    let key = speaker.key.clone();
    sender.add_speaker(speaker, options());
    assert!(playing(&mut events, &sender, &key, 1));
    let count = |server: &Ap2Server, wanted: &str| {
        server.fake().keys().iter().filter(|k| *k == wanted).count()
    };
    assert!(
        udp.data.wait_for(|_| true).is_some(),
        "no audio after playing; FLUSH x{}; {:?}",
        count(&server, "FLUSH"),
        {
            events.drain(&sender);
            events.of(&key)
        }
    );

    // Paused for a while: no audio, the feedback goes on.
    server
        .fake()
        .pause
        .insert("POST /feedback".to_owned(), SLOW_FEEDBACK);
    let feedback = count(&server, "POST /feedback");
    sender.pause();
    let paused = Instant::now();
    assert!(wait_until(LONG, || count(&server, "FLUSH") == 2));
    assert!(wait_until(LONG, || count(&server, "POST /feedback") >= feedback + 2));
    let (seq, rtptime) = rtp_info(&server);
    // The FLUSH names the first packet held back. Packets before it left
    // before the sender took the pause, however late a busy runner stamps
    // them; nothing else arrives.
    let datagrams = udp.data.take();
    let late = tail_before(&datagrams, paused, seq).len();
    assert_eq!(
        datagrams.iter().filter(|(at, _, _)| *at > paused).count(),
        late
    );
    assert_eq!(count(&server, "FLUSHBUFFERED"), 0);
    sender.resume();
    let (_, _, bytes) = udp.data.wait_for(|_| true).expect("audio after the resume");
    assert_eq!(seq_of(&bytes), seq);
    assert_eq!(bytes[1], 0xE0);
    assert_eq!(u32_at(&bytes, 4), rtptime);

    // A delay FLUSH in flight when the pause comes: only the pause FLUSH's
    // answer, about 2 ANSWER after the pause, lets the audio out.
    server.fake().pause.remove("POST /feedback");
    server.fake().pause.insert("FLUSH".to_owned(), ANSWER);
    let flushes = server.fake().flushes_seen.clone();
    sender.set_delay(&key, Duration::from_millis(200));
    // A delay FLUSH still queued behind another request would merge with
    // the pause FLUSH, so the pause waits until the speaker has read it.
    assert!(wait_until(LONG, || flushes.load(Ordering::SeqCst) == 3));
    udp.data.take();
    sender.pause();
    let paused = Instant::now();
    sender.resume();
    assert!(wait_until(LONG, || count(&server, "FLUSH") == 4));
    let held = rtp_info(&server).0;
    let (at, _, bytes) = udp
        .data
        .wait_for(|(_, _, bytes)| !before(seq_of(bytes), held))
        .expect("audio after the resume");
    assert!(
        at >= paused + ANSWER + ANSWER / 2,
        "audio {:?} after the pause",
        at - paused
    );
    assert_eq!(count(&server, "FLUSH"), 4);
    assert_eq!(seq_of(&bytes), held);
    assert_eq!(bytes[1], 0xE0);
    tail_before(&udp.data.take(), paused, held);
    sender.shutdown();
}

fn ptp_fake() -> FakeAp2 {
    let mut fake = FakeAp2::new();
    fake.info_body = plist(dict(&[
        ("features", int((1 << 38) | (1 << 41) | (1 << 48))),
        ("statusFlags", int(0x4)),
    ]));
    fake.status_once.insert("SETUP session".to_owned(), 501);
    fake
}

const PTP_FEATURES: &str = "0x0,0x10240";

// covers: P158, P167
#[test]
fn ap2_ptp_session() {
    let _serial = serial();
    let udp = Udp::bind();
    let server = Ap2Server::start(udp.fake(ptp_fake()));
    let origin = Instant::now();
    let local = free_ptp_ports();
    let (sender, _host) = start(SenderClock::new(origin), local, udp.ptp_ports());
    let mut events = Events::default();
    let speaker = device(&server, 62, PTP_FEATURES);
    let key = speaker.key.clone();
    sender.add_speaker(speaker, options());
    assert!(
        playing(&mut events, &sender, &key, 1),
        "{:?} {:?}",
        events.of(&key),
        server.fake().keys()
    );
    let keys = server.fake().keys();
    let setups = keys.iter().filter(|k| *k == "SETUP session").count();
    assert_eq!(setups, 2, "NTP refused, then PTP: {keys:?}");
    let setup = server.fake().last("SETUP session").cloned().expect("SETUP");
    let body = read_dict(&setup.body).expect("plist");
    let clock_id = body
        .get("timingPeerInfo")
        .and_then(|info| info.as_dictionary())
        .and_then(|info| info.get("ClockID"))
        .and_then(|id| id.as_signed_integer())
        .expect("ClockID");
    let identity = clock_id.to_be_bytes();
    assert_eq!(&identity[..2], &[0xFF, 0xFF]);
    assert!(!ports_free(local), "the PTP ports are held under PTP");

    // Announce, Sync and Follow_Up reach the receiver's ports, from the
    // sender's ports 319 and 320 (here the test's own), by unicast. The
    // window starts at an Announce and is shorter than their 1 s period, as
    // when a loaded sender sends the next one late.
    let kinds = |got: &[(Instant, SocketAddr, Vec<u8>)], kind: u8| {
        got.iter().filter(|(_, _, b)| b[0] & 0x0F == kind).count()
    };
    assert!(
        udp.general
            .wait_for(|(_, _, b)| b[0] & 0x0F == 0xB)
            .is_some()
    );
    udp.general.take();
    udp.event.take();
    std::thread::sleep(Duration::from_millis(900));
    let (general, event) = (udp.general.take(), udp.event.take());
    assert!(kinds(&general, 0xB) >= 1, "Announce");
    assert!(
        kinds(&general, 0x8) >= 6,
        "Follow_Up x{} in 0.9 s",
        kinds(&general, 0x8)
    );
    assert!((6..=10).contains(&kinds(&event, 0x0)), "Sync");
    for (_, from, bytes) in general.iter().chain(&event) {
        assert_eq!(&bytes[20..28], &identity);
        assert!(from.ip().is_loopback());
    }
    assert!(event.iter().all(|(_, from, _)| from.port() == local.event));
    assert!(
        general
            .iter()
            .all(|(_, from, _)| from.port() == local.general)
    );
    // The Follow_Up time is the sender clock's PTP time (P163).
    let (arrived, _, follow) = general
        .iter()
        .rfind(|(_, _, b)| b[0] & 0x0F == 0x8)
        .expect("Follow_Up");
    let (seconds, nanos) = timestamp(&follow[34..44]);
    let stated = seconds as f64 + f64::from(nanos) / 1e9;
    let actual = arrived.duration_since(origin).as_secs_f64();
    assert!((actual - stated).abs() < 0.1, "{actual} {stated}");

    // A Delay_Req from the receiver gets its Delay_Resp on port 320.
    let request = message(0x1, [0x02, 0, 0, 0xFF, 0xFE, 0, 0, 0x02], 77, 44);
    udp.general.take();
    udp.event
        .socket
        .send_to(&request, (Ipv4Addr::LOCALHOST, local.event))
        .expect("Delay_Req");
    let answers = udp.general.after(Duration::from_millis(400));
    let resp = answers
        .iter()
        .find(|(_, _, b)| b[0] & 0x0F == 0x9)
        .expect("Delay_Resp");
    assert_eq!(header(&resp.2).seq, 77);

    // Time announces take the place of sync packets, with the clock identity.
    let control = udp.control.fresh(Duration::from_millis(1_200));
    let announces: Vec<_> = control
        .iter()
        .filter(|(_, _, b)| b.len() == 28 && b[1] == 0xD7)
        .collect();
    assert!(!announces.is_empty());
    assert!(control.iter().all(|(_, _, b)| b[1] != 0xD4));
    assert_eq!(&announces[0].2[20..28], &identity);
    let shk = stream_key(&server.fake()).expect("shk");
    let audio = udp.data.fresh(Duration::from_millis(300));
    assert!(open_packet(&shk, &audio[0].2).is_some());

    // The session ends: the PTP traffic stops.
    sender.remove_speaker(&key);
    assert!(wait_until(LONG, || server
        .fake()
        .keys()
        .iter()
        .filter(|k| *k == "TEARDOWN")
        .count()
        == 2));
    std::thread::sleep(Duration::from_millis(300));
    udp.event.take();
    udp.general.take();
    std::thread::sleep(Duration::from_millis(400));
    assert!(udp.event.take().is_empty());
    assert!(udp.general.take().is_empty());
    assert!(wait_until(LONG, || ports_free(local)));
    sender.shutdown();
}

// covers: P158
#[test]
fn ap2_ntp_lets_the_ptp_ports_go() {
    let _serial = serial();
    let udp = Udp::bind();
    let mut fake = ptp_fake();
    fake.status_once.clear();
    let server = Ap2Server::start(udp.fake(fake));
    let local = free_ptp_ports();
    let (sender, _host) = start(SenderClock::new(Instant::now()), local, udp.ptp_ports());
    let mut events = Events::default();
    let speaker = device(&server, 64, PTP_FEATURES);
    let key = speaker.key.clone();
    sender.add_speaker(speaker, options());
    assert!(
        playing(&mut events, &sender, &key, 1),
        "{:?} {:?}",
        events.of(&key),
        server.fake().keys()
    );
    let keys = server.fake().keys();
    assert_eq!(keys.iter().filter(|k| *k == "SETUP session").count(), 1);
    // The receiver took NTP, so another PTP stack may have 319 and 320.
    assert!(wait_until(LONG, || ports_free(local)));
    let control = udp.control.fresh(Duration::from_millis(1_200));
    assert!(
        control
            .iter()
            .any(|(_, _, b)| b.len() == 20 && b[1] == 0xD4)
    );
    assert!(udp.event.take().is_empty());
    assert!(udp.general.take().is_empty());
    sender.shutdown();
}

// covers: P158
#[test]
fn ap2_ptp_unavailable() {
    let _serial = serial();
    let udp = Udp::bind();
    let mut fake = ptp_fake();
    fake.status.insert("SETUP session".to_owned(), 501);
    let server = Ap2Server::start(udp.fake(fake));
    let local = free_ptp_ports();
    // Another PTP stack holds the event port.
    let _holder = UdpSocket::bind((ptp_bind_v4(), local.event)).expect("the port");
    let (sender, _host) = start(SenderClock::new(Instant::now()), local, udp.ptp_ports());
    let mut events = Events::default();
    let speaker = device(&server, 63, PTP_FEATURES);
    let key = speaker.key.clone();
    sender.add_speaker(speaker.clone(), options());
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Failed)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    let errors: Vec<ErrorKind> = events
        .of(&key)
        .into_iter()
        .filter_map(|event| match event {
            SpeakerEvent::Error(error) => Some(error.error.kind),
            _ => None,
        })
        .collect();
    assert_eq!(
        errors,
        vec![ErrorKind::Unsupported(UnsupportedReason::Timing)]
    );
    let keys = server.fake().keys();
    assert_eq!(keys.iter().filter(|k| *k == "SETUP session").count(), 1);

    // Once more: the host was told already.
    sender.remove_speaker(&key);
    std::thread::sleep(Duration::from_millis(200));
    sender.add_speaker(speaker, options());
    assert!(
        events
            .wait_state(&sender, &key, 2, LONG, |s| *s == SpeakerState::Failed)
            .is_some(),
        "{:?}",
        events.of(&key)
    );
    let told = events
        .of(&key)
        .into_iter()
        .filter(|event| *event == SpeakerEvent::Warning(Warning::PtpUnavailable))
        .count();
    assert_eq!(told, 1);
    assert!(udp.event.after(Duration::from_millis(200)).is_empty());
    assert!(udp.general.take().is_empty());
    sender.shutdown();
}

// covers: P158
#[test]
fn ap2_ptp_clash_is_told_again_after_ptp_bound() {
    let _serial = serial();
    let udp = Udp::bind();
    let mut fake = ptp_fake();
    fake.status.insert("SETUP session".to_owned(), 501);
    let server = Ap2Server::start(udp.fake(fake));
    let local = free_ptp_ports();
    let holder = UdpSocket::bind((ptp_bind_v4(), local.event)).expect("the port");
    let (sender, _host) = start(SenderClock::new(Instant::now()), local, udp.ptp_ports());
    let mut events = Events::default();
    let speaker = device(&server, 65, PTP_FEATURES);
    let key = speaker.key.clone();
    let told = |events: &Events| {
        events
            .of(&key)
            .into_iter()
            .filter(|event| *event == SpeakerEvent::Warning(Warning::PtpUnavailable))
            .count()
    };
    let opened = |events: &Events| {
        events
            .of(&key)
            .into_iter()
            .filter(|event| *event == SpeakerEvent::PtpOpened)
            .count()
    };
    let attempt = |events: &mut Events, n: usize| {
        sender.add_speaker(speaker.clone(), options());
        assert!(
            events
                .wait_state(&sender, &key, n, LONG, |s| *s == SpeakerState::Failed)
                .is_some(),
            "{:?}",
            events.of(&key)
        );
        sender.remove_speaker(&key);
        std::thread::sleep(Duration::from_millis(200));
    };
    attempt(&mut events, 1);
    assert_eq!((told(&events), opened(&events)), (1, 0));
    // The other app lets go: the next session binds PTP and says so.
    drop(holder);
    attempt(&mut events, 2);
    assert_eq!((told(&events), opened(&events)), (1, 1));
    // It takes the port again: the clash is told once more.
    assert!(wait_until(LONG, || ports_free(local)));
    let _holder = UdpSocket::bind((ptp_bind_v4(), local.event)).expect("the port");
    attempt(&mut events, 3);
    assert_eq!((told(&events), opened(&events)), (2, 1));
    sender.shutdown();
}

/// A speaker whose TXT record also carries `srcvers`.
fn versioned(server: &Ap2Server, id: u8, features: &str, srcvers: &str) -> Device {
    let mut speaker = device(server, id, features);
    if let Some(txt) = speaker.airplay_txt.as_mut() {
        txt.source_version = Some(srcvers.to_owned());
    }
    speaker
}

/// One launch of a fresh sender, as after a relaunch: the host hands the
/// speaker `stored` from its store and the launch ends once the speaker is
/// `until`. The timing each session SETUP asked for, and the speaker's
/// events.
fn launch(
    server: &Ap2Server,
    udp: &Udp,
    speaker: &Device,
    stored: Option<TimingMemory>,
    until: SpeakerState,
) -> (Vec<String>, Vec<SpeakerEvent>) {
    server.fake().requests.clear();
    let (sender, _host) = start(
        SenderClock::new(Instant::now()),
        free_ptp_ports(),
        udp.ptp_ports(),
    );
    let mut events = Events::default();
    let key = speaker.key.clone();
    let mut with_store = options();
    with_store.timing = stored;
    sender.add_speaker(speaker.clone(), with_store);
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == until)
            .is_some(),
        "{:?} {:?}",
        events.of(&key),
        server.fake().keys()
    );
    sender.shutdown();
    let timings = server
        .fake()
        .requests
        .iter()
        .filter(|(request, _)| request_key(request) == "SETUP session")
        .filter_map(|(request, _)| {
            read_dict(&request.body)?
                .get("timingProtocol")?
                .as_string()
                .map(str::to_owned)
        })
        .collect();
    (timings, events.of(&key))
}

/// The last entry the sender handed the host to keep.
fn remembered(events: &[SpeakerEvent]) -> Option<TimingMemory> {
    events.iter().rev().find_map(|event| match event {
        SpeakerEvent::TimingRemembered(memory) => Some(memory.clone()),
        _ => None,
    })
}

// covers: P217
#[test]
fn ap2_timing_choice_outlives_the_launch() {
    let _serial = serial();
    let udp = Udp::bind();
    let mut fake = ptp_fake();
    fake.status_once.clear();
    fake.refuse_timing.push("NTP".to_owned());
    let server = Ap2Server::start(udp.fake(fake));
    let speaker = versioned(&server, 65, PTP_FEATURES, "366.0");

    // Nothing stored: NTP is refused, PTP takes it, and the host gets the
    // choice with the TXT values of that SETUP.
    let (timings, events) = launch(&server, &udp, &speaker, None, SpeakerState::Playing);
    assert_eq!(timings, ["NTP", "PTP"]);
    let stored = remembered(&events).expect("the choice reaches the host");
    assert_eq!(
        stored,
        TimingMemory {
            timing: Timing::Ptp,
            features: speaker.features().0,
            srcvers: Some("366.0".to_owned()),
        }
    );

    // The next launch with the stored entry asks for PTP at once: one NTP
    // probe across both launches.
    let (timings, events) = launch(
        &server,
        &udp,
        &speaker,
        Some(stored.clone()),
        SpeakerState::Playing,
    );
    assert_eq!(timings, ["PTP"]);
    assert!(!events.contains(&SpeakerEvent::TimingForgotten));

    // A new srcvers forgets the entry, so the NTP attempt comes back once.
    let updated = versioned(&server, 65, PTP_FEATURES, "377.1");
    let (timings, events) = launch(
        &server,
        &udp,
        &updated,
        Some(stored.clone()),
        SpeakerState::Playing,
    );
    assert_eq!(timings, ["NTP", "PTP"]);
    assert!(events.contains(&SpeakerEvent::TimingForgotten));
    let renewed = remembered(&events).expect("the new entry");
    assert_eq!(renewed.timing, Timing::Ptp);
    assert_eq!(renewed.srcvers.as_deref(), Some("377.1"));
    let (timings, _) = launch(
        &server,
        &udp,
        &updated,
        Some(renewed),
        SpeakerState::Playing,
    );
    assert_eq!(timings, ["PTP"]);

    // So do new features.
    let changed = versioned(&server, 65, "0x0,0x10241", "366.0");
    let (timings, events) = launch(&server, &udp, &changed, Some(stored), SpeakerState::Playing);
    assert_eq!(timings, ["NTP", "PTP"]);
    assert!(events.contains(&SpeakerEvent::TimingForgotten));
    assert_eq!(
        remembered(&events).map(|memory| memory.features),
        Some(changed.features().0)
    );
}

// covers: P217
#[test]
fn ap2_remembered_ptp_refused_is_forgotten() {
    let _serial = serial();
    let udp = Udp::bind();
    // The receiver now takes NTP and refuses PTP.
    let mut fake = ptp_fake();
    fake.status_once.clear();
    fake.refuse_timing.push("PTP".to_owned());
    let server = Ap2Server::start(udp.fake(fake));
    let speaker = versioned(&server, 66, PTP_FEATURES, "366.0");
    let stored = TimingMemory {
        timing: Timing::Ptp,
        features: speaker.features().0,
        srcvers: Some("366.0".to_owned()),
    };
    let (timings, events) = launch(
        &server,
        &udp,
        &speaker,
        Some(stored.clone()),
        SpeakerState::Playing,
    );
    assert_eq!(timings, ["PTP", "NTP"]);
    let forgotten = events
        .iter()
        .position(|event| *event == SpeakerEvent::TimingForgotten)
        .expect("the entry is dropped");
    let renewed = events
        .iter()
        .position(|event| {
            matches!(event, SpeakerEvent::TimingRemembered(memory) if memory.timing == Timing::Ntp)
        })
        .expect("NTP becomes the entry");
    assert!(forgotten < renewed, "{events:?}");

    // Refused under both: Unsupported (timing), and no entry is kept.
    server.fake().refuse_timing.push("NTP".to_owned());
    let (timings, events) = launch(&server, &udp, &speaker, Some(stored), SpeakerState::Failed);
    assert_eq!(timings, ["PTP", "NTP", "PTP"]);
    assert!(events.contains(&SpeakerEvent::TimingForgotten));
    assert_eq!(remembered(&events), None);
    assert!(events.iter().any(|event| matches!(
        event,
        SpeakerEvent::Error(error)
            if error.error.kind == ErrorKind::Unsupported(UnsupportedReason::Timing)
    )));
}
