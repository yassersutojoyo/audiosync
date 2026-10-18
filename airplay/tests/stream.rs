//! The stream against fake receivers on a fake clock: RTP packets and
//! numbering, shared payloads, pacing and the margin, sync packets and the
//! latency they state, per-speaker delay, joining, flushes, restarts,
//! retransmission, software volume, buffers and counters (sections 5 to 7
//! and 10 of docs/AIRPLAY_SPEC.md).

mod common;

use std::net::{IpAddr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::time::Duration;

use audiosync_airplay::audio::{FRAMES_PER_PACKET, Frame};
use audiosync_airplay::rtp::{RESEND_PREFIX_BYTES, ResendRequest, SyncPacket};
use audiosync_airplay::sdp::Codec;
use audiosync_airplay::session::{MediaDestinations, ReceiverPorts};
use audiosync_airplay::store::{DELAY_PACKETS, HISTORY_PACKETS, STORE_PACKETS};
use audiosync_airplay::stream::{
    DEFAULT_LATENCY_FRAMES, LocalSocket, PacketKind, SpeakerId, StreamConfig, StreamEvent,
};
use audiosync_airplay::volume::Volume;
use common::rig::{Host, Rig, decode, destinations, host_frame, speaker};

const PACKET: u64 = FRAMES_PER_PACKET as u64;

/// The timeline packet an RTP timestamp of a speaker belongs to.
fn packet_of(rig: &Rig, id: SpeakerId, rtp: u32) -> u64 {
    let n = rig.frame_of(id, rtp);
    assert_eq!(
        n.rem_euclid(PACKET as i64),
        0,
        "packets start at multiples of 352"
    );
    n as u64 / PACKET
}

/// When packet j is due for a speaker with delay `delay` (P84).
fn due(rig: &Rig, j: u64, margin: f64, delay: f64) -> f64 {
    rig.c((j * PACKET + PACKET - 1) as i64) + margin + delay
}

fn flushes(rig: &Rig, id: SpeakerId) -> Vec<(u16, u32)> {
    rig.events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Flush {
                speaker,
                seq,
                rtptime,
            } if *speaker == id => Some((*seq, *rtptime)),
            _ => None,
        })
        .collect()
}

fn stated(rig: &Rig, sync: &SyncPacket) -> f64 {
    rig.clock.seconds_of(sync.time)
}

// covers: P75, P78, P79, P83
#[test]
fn rtp_header_and_numbering() {
    let mut rig = Rig::with_host();
    let mut config = speaker(1, Codec::Alac);
    config.first_seq = 65_530;
    config.first_rtptime = u32::MAX - 1_000;
    let id = rig.add(config);
    rig.run_for(Duration::from_secs(1));

    let sent = rig.of(id);
    let audio: Vec<_> = sent
        .iter()
        .filter(|s| s.transmit.kind == PacketKind::Audio)
        .collect();
    assert!(audio.len() > 100);
    // 0x80, then 0xE0 on the first packet after RECORD and 0x60 after
    // (P78); the SSRC stays.
    assert_eq!(&audio[0].transmit.bytes[..2], &[0x80, 0xE0]);
    for packet in &audio[1..] {
        assert_eq!(&packet.transmit.bytes[..2], &[0x80, 0x60]);
    }
    let headers = rig.audio(id);
    // The first packet carries RECORD's values; each later one adds 1 to
    // the sequence number and 352 to the timestamp, both wrapping (P79).
    assert_eq!(headers[0].header.seq, 65_530);
    assert_eq!(headers[0].header.timestamp, u32::MAX - 1_000);
    for pair in headers.windows(2) {
        assert_eq!(pair[1].header.seq, pair[0].header.seq.wrapping_add(1));
        assert_eq!(
            pair[1].header.timestamp,
            pair[0].header.timestamp.wrapping_add(352)
        );
        assert_eq!(pair[1].header.ssrc, 0x5EED_1001);
    }
    assert!(headers.iter().any(|a| a.header.seq == 0), "seq wrapped");
    assert!(
        headers.iter().any(|a| a.header.timestamp < 1_000),
        "rtptime wrapped"
    );
    // 352 frames in every packet (P75).
    for packet in &headers {
        assert_eq!(decode(Codec::Alac, &packet.payload).len(), 352);
    }

    // Audio to server_port, sync to control_port, all from the control
    // socket (P83).
    for sent in &sent {
        assert_eq!(sent.transmit.from, LocalSocket::Control);
        let expected = match sent.transmit.kind {
            PacketKind::Audio => destinations(1).audio,
            PacketKind::Sync | PacketKind::Resend => destinations(1).control,
            PacketKind::Timing => panic!("no timing packets"),
        };
        assert_eq!(sent.transmit.to, expected);
    }
    assert_eq!(destinations(1).audio.port(), 50_000);
    assert_eq!(destinations(1).control.port(), 50_001);

    // The UDP traffic follows the address the session connected to, scope
    // included (P17).
    let scoped = SocketAddr::V6(SocketAddrV6::new(
        Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0x21),
        7000,
        0,
        4,
    ));
    let mut config = speaker(2, Codec::Alac);
    config.destinations = MediaDestinations::new(
        scoped,
        ReceiverPorts {
            server_port: 50_010,
            control_port: 50_011,
            timing_port: 0,
        },
    );
    let other = rig.add(config);
    rig.run_for(Duration::from_millis(200));
    for sent in rig.of(other) {
        let SocketAddr::V6(to) = sent.transmit.to else {
            panic!("IPv6");
        };
        assert_eq!(IpAddr::V6(*to.ip()), scoped.ip());
        assert_eq!(to.scope_id(), 4);
    }
}

// covers: P80, P81
#[test]
fn shared_payload_many_speakers() {
    let mut rig = Rig::with_host();
    let ids: Vec<SpeakerId> = (1..=3).map(|k| rig.add(speaker(k, Codec::Alac))).collect();
    let pcm = rig.add(speaker(4, Codec::Pcm));
    rig.run_for(Duration::from_secs(2));

    // One encode per packet, however many speakers play it (P80).
    let counters = rig.stream.counters();
    assert_eq!(counters.packets_encoded(), rig.stream.next_packet());
    let first = rig.audio(ids[0]);
    assert!(first.len() > 200);
    for &other in &ids[1..] {
        let audio = rig.audio(other);
        for packet in &audio {
            let j = packet_of(&rig, other, packet.header.timestamp);
            let same = first
                .iter()
                .find(|a| packet_of(&rig, ids[0], a.header.timestamp) == j)
                .expect("same packet");
            // The same contents, its own header.
            assert_eq!(packet.payload, same.payload);
            assert_ne!(packet.header.ssrc, same.header.ssrc);
            assert_ne!(packet.header.seq, same.header.seq);
            assert_ne!(packet.header.timestamp, same.header.timestamp);
        }
    }
    // The PCM speaker plays the same frames.
    for packet in rig.audio(pcm).iter().take(50) {
        let j = packet_of(&rig, pcm, packet.header.timestamp);
        let same = first
            .iter()
            .find(|a| packet_of(&rig, ids[0], a.header.timestamp) == j)
            .expect("same packet");
        assert_eq!(
            decode(Codec::Pcm, &packet.payload),
            decode(Codec::Alac, &same.payload)
        );
    }

    // Frame n has RTP timestamp k_s + n for every speaker (P81): the first
    // packet carries RECORD's value at a packet boundary, and the timestamp
    // of every packet is k_s + 352·j.
    for (k, &id) in ids.iter().chain([&pcm]).enumerate() {
        let config = speaker(k as u8 + 1, Codec::Alac);
        let (packet, rtptime, _) = rig.first_sounds(id)[0];
        assert_eq!(rtptime, config.first_rtptime);
        let offset = rtptime.wrapping_sub((packet * PACKET) as u32);
        for audio in rig.audio(id) {
            let j = packet_of(&rig, id, audio.header.timestamp);
            assert_eq!(
                audio.header.timestamp,
                offset.wrapping_add((j * PACKET) as u32)
            );
            // And it carries the host's frames 352·j on.
            let frames = decode(
                if id == pcm { Codec::Pcm } else { Codec::Alac },
                &audio.payload,
            );
            assert_eq!(frames[0], host_frame(j * PACKET));
        }
    }
}

// covers: P85
#[test]
fn silence_when_idle() {
    let mut rig = Rig::with_host();
    rig.host.as_mut().expect("host").stop_at = Some(100); // 1 s of audio
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(3));

    let audio = rig.audio(id);
    // Packets never pause: no gap in the numbering, none in time.
    for pair in audio.windows(2) {
        assert_eq!(pair[1].header.seq, pair[0].header.seq.wrapping_add(1));
        assert!(pair[1].at - pair[0].at < 0.008 + 0.005, "{}", pair[1].at);
    }
    assert!(
        audio.last().expect("audio").at > 2.9,
        "still playing at the end"
    );
    // After the host stopped, they carry digital silence, counted.
    let silent_packets = audio
        .iter()
        .filter(|a| a.at > 1.2)
        .map(|a| decode(Codec::Alac, &a.payload))
        .filter(|frames| frames.iter().all(|f| *f == [0, 0]))
        .count();
    assert!(silent_packets > 200);
    let counters = rig.stream.counters();
    assert!(counters.silent_frames() >= 200 * PACKET);
    let speaker_counters = rig.stream.speaker_counters(id).expect("speaker");
    assert!(speaker_counters.silent_frames() >= 200 * PACKET);
    assert_eq!(counters.input_frames_dropped(), 0);
}

// covers: P112
#[test]
fn underrun_and_overrun() {
    // A capture thread stalls 60 ms on block 100 and then delivers the
    // backlog at once: the packets whose time came are sent on time with
    // silence, the late frames are dropped, and everything after lands
    // where it belongs.
    let mut rig = Rig::with_host();
    rig.host.as_mut().expect("host").stalls = vec![(100, 0.060)];
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(3));
    let counters = rig.stream.counters();
    let silent = counters.silent_frames();
    assert!(silent > 0, "the stall left silence");
    assert_eq!(
        counters.input_frames_dropped(),
        silent,
        "every frame that came too late was dropped, no other"
    );
    let audio = rig.audio(id);
    for pair in audio.windows(2) {
        assert_eq!(pair[1].header.seq, pair[0].header.seq.wrapping_add(1));
    }
    for packet in &audio {
        let n = rig.frame_of(id, packet.header.timestamp) as u64;
        for (offset, frame) in decode(Codec::Alac, &packet.payload).iter().enumerate() {
            if *frame != [0, 0] {
                assert_eq!(*frame, host_frame(n + offset as u64), "no shift");
            }
        }
    }
    let late = rig.stream.speaker_counters(id).expect("counters");
    assert_eq!(late.late_packets(), 0, "sent on time");

    // A write larger than the free ring keeps what fits and counts the
    // rest.
    let mut rig = Rig::new(StreamConfig::default());
    let samples: Vec<i16> = (0..3 * 44_100u64).flat_map(host_frame).collect();
    let kept = rig.writer.write_i16(&samples, rig.now);
    assert_eq!(kept, rig.stream.input_frames());
    rig.run_for(Duration::from_millis(100));
    assert_eq!(
        rig.stream.counters().input_frames_dropped(),
        3 * 44_100 - kept as u64
    );
    // What fit plays from frame 0 on.
    let id = rig.add(speaker(1, Codec::Pcm));
    rig.run_for(Duration::from_millis(100));
    let packet = &rig.audio(id)[0];
    let n = rig.frame_of(id, packet.header.timestamp) as u64;
    assert_eq!(decode(Codec::Pcm, &packet.payload)[0], host_frame(n));
}

// covers: P111
#[test]
fn buffers_are_sized_by_the_spec_and_allocated_at_the_start() {
    // At least 1 s of input ring, whatever is asked for.
    let rig = Rig::new(StreamConfig {
        latency_frames: DEFAULT_LATENCY_FRAMES,
        input_frames: 100,
    });
    assert_eq!(rig.stream.input_frames(), 44_100);
    let mut rig = Rig::with_host();
    assert_eq!(rig.stream.input_frames(), 2 * 44_100);
    // One payload store per codec covers the resend history plus the
    // largest delay (and M): 1024 + 376 packets and a little more.
    assert_eq!(DELAY_PACKETS, 376);
    assert!(rig.stream.store_packets() >= HISTORY_PACKETS + DELAY_PACKETS);
    assert_eq!(rig.stream.store_packets(), STORE_PACKETS);
    // A speaker with its own encoding gets a store of the same size.
    let mut config = speaker(1, Codec::Alac);
    config.software_volume = Some(Volume::Db(-10.0));
    let own = rig.add(config);
    let plain = rig.add(speaker(2, Codec::Alac));
    assert_eq!(rig.stream.own_store_packets(own), Some(STORE_PACKETS));
    assert_eq!(rig.stream.own_store_packets(plain), None);
    // A delayed speaker finds its packets 3 s later.
    let mut config = speaker(3, Codec::Alac);
    config.delay = Duration::from_millis(3000);
    let delayed = rig.add(config);
    rig.run_for(Duration::from_secs(6));
    let audio = rig.audio(delayed);
    assert!(audio.len() > 300);
    for packet in &audio {
        let n = rig.frame_of(delayed, packet.header.timestamp) as u64;
        assert_eq!(decode(Codec::Alac, &packet.payload)[0], host_frame(n));
    }
}

// covers: P84, P99
#[test]
fn pacing() {
    let mut rig = Rig::with_host();
    let fast = rig.add(speaker(1, Codec::Alac));
    let mut config = speaker(2, Codec::Alac);
    config.delay = Duration::from_millis(500);
    let slow = rig.add(config);
    rig.run_for(Duration::from_secs(5));

    for (id, delay) in [(fast, 0.0), (slow, 0.5)] {
        let audio = rig.audio(id);
        assert!(audio.len() > 400);
        for packet in &audio {
            let j = packet_of(&rig, id, packet.header.timestamp);
            let due = due(&rig, j, packet.margin, delay);
            // Within 5 ms of c(352·j + 351) + M + d_s, never more than
            // 20 ms early.
            assert!((packet.at - due).abs() <= 0.005, "packet {j}");
            assert!(packet.at >= due - 0.020);
        }
    }
    // The delayed speaker sends each packet 500 ms later (P99).
    let fast_audio = rig.audio(fast);
    for packet in rig.audio(slow) {
        let j = packet_of(&rig, slow, packet.header.timestamp);
        if let Some(same) = fast_audio
            .iter()
            .find(|a| packet_of(&rig, fast, a.header.timestamp) == j)
        {
            assert!((packet.at - same.at - 0.5).abs() <= 0.005);
        }
    }
    // M stays between 20 and 100 ms.
    for &(_, margin) in &rig.margins {
        assert!((0.020..=0.100).contains(&margin), "{margin}");
    }
}

// covers: P84, P112
#[test]
fn pacing_with_jittered_blocks() {
    // 10 ms blocks with ±5 ms arrival jitter, d_s = 0, for 60 s.
    let mut rig = Rig::with_host();
    let mut host = Host::new(0.0).seed(2026);
    host.jitter = 0.005;
    rig.host = Some(host);
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(60));
    let counters = rig.stream.counters();
    assert_eq!(counters.silent_frames(), 0, "no silent frames inserted");
    assert_eq!(counters.input_frames_dropped(), 0, "no frames dropped");
    let speaker_counters = rig.stream.speaker_counters(id).expect("counters");
    assert_eq!(speaker_counters.late_packets(), 0);
    assert!(speaker_counters.packets_sent() > 7_400);
    // M settles between 20 and 30 ms.
    for &(t, margin) in &rig.margins {
        if t > 30.0 {
            assert!((0.020..=0.030).contains(&margin), "M = {margin} at {t}");
        }
    }
    // The audio is intact.
    for packet in rig.audio(id).iter().step_by(97) {
        let n = rig.frame_of(id, packet.header.timestamp) as u64;
        let frames = decode(Codec::Alac, &packet.payload);
        let expected: Vec<Frame> = (n..n + PACKET).map(host_frame).collect();
        assert_eq!(frames, expected);
    }
}

// covers: P93, P94
#[test]
fn sync_packet_layout() {
    let mut rig = Rig::with_host();
    let first = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_millis(1_300));
    let late = rig.add(speaker(2, Codec::Alac));
    rig.run_for(Duration::from_millis(1_900));

    for id in [first, late] {
        let sent = rig.of(id);
        let mut syncs = 0u16;
        let mut synced_packets = Vec::new();
        for (index, item) in sent.iter().enumerate() {
            if item.transmit.kind != PacketKind::Sync {
                continue;
            }
            let bytes = &item.transmit.bytes;
            assert_eq!(bytes.len(), 20);
            assert_eq!(bytes[0], if syncs == 0 { 0x90 } else { 0x80 });
            assert_eq!(bytes[1], 0xD4);
            let sync = SyncPacket::parse(bytes).expect("sync");
            assert_eq!(sync.seq, syncs, "counts up from 0");
            syncs += 1;
            // It goes just before the packet whose timestamp it carries.
            let next = &sent[index + 1];
            assert_eq!(next.transmit.kind, PacketKind::Audio);
            assert_eq!(next.at, item.at);
            let header =
                audiosync_airplay::rtp::RtpHeader::parse(&next.transmit.bytes).expect("audio");
            assert_eq!(header.timestamp, sync.next_rtp);
            synced_packets.push(packet_of(&rig, id, header.timestamp));
        }
        // One before the first packet, then one before the first packet that
        // starts at or after each multiple of 44 100 frames.
        let audio = rig.audio(id);
        let first_packet = packet_of(&rig, id, audio[0].header.timestamp);
        let last_packet = packet_of(&rig, id, audio.last().expect("audio").header.timestamp);
        let mut expected = vec![first_packet];
        for j in first_packet + 1..=last_packet {
            if (j * PACKET) / 44_100 != ((j - 1) * PACKET) / 44_100 {
                expected.push(j);
            }
        }
        assert_eq!(synced_packets, expected);
        assert!(expected.len() >= 2);
    }
    // Packet 126 is the first to start at or after frame 44 100.
    assert!(rig.audio(first).len() > 130);
}

// covers: P95, P97, P98
#[test]
fn sync_latency_and_times() {
    let mut rig = Rig::with_host();
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(4));
    assert_eq!(rig.stream.latency_frames(), 77_175, "1.75 s by default");
    assert!((rig.latency() - 1.75).abs() < 1e-9);
    let syncs = rig.syncs(id);
    assert!(syncs.len() >= 4);
    for (at, sync) in &syncs {
        // The latency asked for: bytes 16–19 minus 4–7 is Lf (P95).
        assert_eq!(sync.next_rtp.wrapping_sub(sync.rtp_heard), 77_175);
        // Bytes 8–15 are P_s(352·j − Lf) from the model (P97).
        let n = rig.frame_of(id, sync.rtp_heard);
        let expected = rig.c(n) + rig.latency();
        assert!((stated(&rig, sync) - expected).abs() < 1e-6);
        // The stated time lies within about M + 10 ms of the send time.
        let margin = rig.stream.margin().as_secs_f64();
        let lead = at - stated(&rig, sync);
        assert!((0.0..=margin + 0.010).contains(&lead), "{lead}");
        // Read the receiver's way (P95): the next packet's first frame is
        // heard Lf frames after the stated one, at P_s(352·j).
        let next = rig.frame_of(id, sync.next_rtp);
        let heard = stated(&rig, sync) + 77_175.0 / 44_100.0;
        assert!((heard - (rig.c(next - 77_175) + 2.0 * rig.latency())).abs() < 1e-6);
        assert!((heard - (rig.c(next) + rig.latency())).abs() < 0.001);
    }

    // The host may set 11 025 to 88 200 frames (P98).
    for (asked, got) in [(5_000, 11_025), (100_000, 88_200), (44_100, 44_100)] {
        let rig = Rig::new(StreamConfig {
            latency_frames: asked,
            input_frames: 88_200,
        });
        assert_eq!(rig.stream.latency_frames(), got);
    }
    // A change flushes every speaker, and the syncs state the new value.
    let other = rig.add(speaker(2, Codec::Alac));
    rig.run_for(Duration::from_millis(500));
    rig.stream.set_latency(200_000, rig.now);
    rig.drain();
    assert_eq!(rig.stream.latency_frames(), 88_200);
    assert_eq!(flushes(&rig, id).len(), 1);
    assert_eq!(flushes(&rig, other).len(), 1);
    assert!(rig.events.iter().any(|event| matches!(
        event,
        StreamEvent::Timeline { restarted: false, latency, .. }
            if (latency.as_secs_f64() - 2.0).abs() < 1e-9
    )));
    let before = rig.syncs(id).len();
    rig.run_for(Duration::from_secs(2));
    let syncs = rig.syncs(id);
    assert!(syncs[before].1.first, "first flag after the flush");
    for (_, sync) in &syncs[before..] {
        assert_eq!(sync.next_rtp.wrapping_sub(sync.rtp_heard), 88_200);
    }
}

// covers: P4, P96, P99, P102
#[test]
fn per_speaker_delay() {
    let mut rig = Rig::with_host();
    let near = rig.add(speaker(1, Codec::Alac));
    let mut config = speaker(2, Codec::Alac);
    config.delay = Duration::from_millis(750);
    let far = rig.add(config);
    rig.run_for(Duration::from_secs(6));

    // Each speaker states P_s(n) = c(n) + L + d_s for the frame it names
    // (P96, P99), with the same latency field (P99).
    let mut stated_by = Vec::new();
    for (id, delay) in [(near, 0.0), (far, 0.75)] {
        let syncs = rig.syncs(id);
        assert!(syncs.len() >= 4);
        let mut points = Vec::new();
        for (_, sync) in &syncs {
            assert_eq!(sync.next_rtp.wrapping_sub(sync.rtp_heard), 77_175);
            let n = rig.frame_of(id, sync.rtp_heard);
            let time = stated(&rig, sync);
            assert!((time - (rig.c(n) + rig.latency() + delay)).abs() < 1e-6);
            points.push((n, time));
        }
        stated_by.push(points);
    }
    // One timeline for both (P4): for every frame the far speaker names,
    // the near one's statements, carried along the shared c(n), put it
    // exactly 750 ms earlier, within one frame (P102).
    for &(n, far_time) in &stated_by[1] {
        let &(m, near_time) = stated_by[0]
            .iter()
            .min_by_key(|(m, _)| (m - n).abs())
            .expect("a near statement");
        let near_for_n = near_time + (rig.c(n) - rig.c(m));
        assert!(
            (far_time - near_for_n - 0.75).abs() < 1.0 / 44_100.0,
            "frame {n}"
        );
    }
}

// covers: P100
#[test]
fn delay_change_flushes_one_speaker() {
    let mut rig = Rig::with_host();
    let steady = rig.add(speaker(1, Codec::Alac));
    let moved = rig.add(speaker(2, Codec::Alac));
    rig.run_for(Duration::from_secs(3));
    let changed_at = rig.t();
    rig.stream
        .set_delay(moved, Duration::from_millis(300), rig.now);
    rig.run_for(Duration::from_millis(100));
    rig.stream
        .set_delay(moved, Duration::from_millis(400), rig.now);
    let last_change = rig.t();
    rig.run_for(Duration::from_millis(200));
    assert!(flushes(&rig, moved).is_empty(), "not before 250 ms");
    rig.run_for(Duration::from_secs(2));

    // Two changes 100 ms apart: one flush, of that speaker only.
    let flush = flushes(&rig, moved);
    assert_eq!(flush.len(), 1);
    assert!(flushes(&rig, steady).is_empty());
    let (seq, rtptime) = flush[0];
    let audio = rig.audio(moved);
    let index = audio
        .iter()
        .position(|a| a.header.seq == seq)
        .expect("the flushed packet");
    let resumed = &audio[index];
    assert!(resumed.header.marker);
    assert_eq!(resumed.header.timestamp, rtptime);
    // Numbering goes on from the last packet before the flush.
    let before = &audio[index - 1];
    assert_eq!(seq, before.header.seq.wrapping_add(1));
    assert_eq!(rtptime, before.header.timestamp.wrapping_add(352));
    assert!(before.at >= changed_at && before.at < last_change + 0.25 + 0.001);
    assert!(resumed.at >= last_change + 0.25 - 0.001);
    // It resumes with the next packet due under the new delay, preceded by
    // a first-flagged sync, and states the new delay.
    let syncs = rig.syncs(moved);
    let after: Vec<_> = syncs.iter().filter(|(at, _)| *at >= resumed.at).collect();
    assert!(after[0].1.first);
    assert_eq!(after[0].1.next_rtp, rtptime);
    for (_, sync) in &after {
        let n = rig.frame_of(moved, sync.rtp_heard);
        let time = stated(&rig, sync);
        assert!((time - (rig.c(n) + rig.latency() + 0.4)).abs() < 1e-6);
    }
    let j = packet_of(&rig, moved, rtptime);
    let flushed_at = last_change + 0.25;
    assert!(due(&rig, j, resumed.margin, 0.4) >= flushed_at - 1e-6);
    assert!(due(&rig, j - 1, resumed.margin, 0.4) < flushed_at);
    // The other speaker is untouched.
    let steady_audio = rig.audio(steady);
    assert!(steady_audio[1..].iter().all(|a| !a.header.marker));
    for pair in steady_audio.windows(2) {
        assert_eq!(pair[1].header.seq, pair[0].header.seq.wrapping_add(1));
    }

    // Changes 300 ms apart are two flushes.
    rig.stream
        .set_delay(moved, Duration::from_millis(100), rig.now);
    rig.run_for(Duration::from_millis(300));
    rig.stream.set_delay(moved, Duration::ZERO, rig.now);
    rig.run_for(Duration::from_millis(300));
    assert_eq!(flushes(&rig, moved).len(), 3);
}

// covers: P103
#[test]
fn join_mid_stream() {
    let mut rig = Rig::with_host();
    let first = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(10));
    let joined_at = rig.t();
    let next_of_first = {
        let audio = rig.audio(first);
        packet_of(&rig, first, audio.last().expect("audio").header.timestamp) + 1
    };
    let joined = rig.add(speaker(2, Codec::Alac));
    rig.run_for(Duration::from_secs(3));

    // It starts with a first-flagged sync, then the next packet in its
    // regular schedule: the one the first speaker sends next too.
    let sent = rig.of(joined);
    assert_eq!(sent[0].transmit.kind, PacketKind::Sync);
    assert!(
        SyncPacket::parse(&sent[0].transmit.bytes)
            .expect("sync")
            .first
    );
    let audio = rig.audio(joined);
    let j = packet_of(&rig, joined, audio[0].header.timestamp);
    assert_eq!(j, next_of_first);
    assert!(due(&rig, j, audio[0].margin, 0.0) >= joined_at - 1e-6);
    assert!(due(&rig, j - 1, audio[0].margin, 0.0) < joined_at);
    // In step with the first speaker (P102): the same statements.
    for (_, sync) in rig.syncs(joined) {
        let n = rig.frame_of(joined, sync.rtp_heard);
        assert!((stated(&rig, &sync) - (rig.c(n) + rig.latency())).abs() < 1e-6);
    }
    // Heard about L − M after it is sent.
    let (_, _, first_sound) = rig.first_sounds(joined)[0];
    let after_send = first_sound - audio[0].at;
    let expected = rig.latency() - audio[0].margin;
    assert!(
        after_send <= expected && after_send >= expected - 0.010,
        "{after_send} vs {expected}"
    );
}

// covers: P109, P110, P113, P115
#[test]
fn latency_reporting() {
    let mut rig = Rig::with_host();
    let mut config = speaker(1, Codec::Alac);
    config.audio_latency = Some(2_205);
    config.delay = Duration::from_millis(250);
    let reported = rig.add(config);
    let plain = rig.add(speaker(2, Codec::Alac));
    rig.run_for(Duration::from_secs(4));

    // The receiver's Audio-Latency is reported, never folded into Lf or
    // the times (P110).
    let latency = rig.stream.speaker_latency(reported).expect("speaker");
    assert_eq!(latency.receiver_latency_frames, Some(2_205));
    for (_, sync) in rig.syncs(reported) {
        assert_eq!(sync.next_rtp.wrapping_sub(sync.rtp_heard), 77_175);
        let n = rig.frame_of(reported, sync.rtp_heard);
        assert!((stated(&rig, &sync) - (rig.c(n) + rig.latency() + 0.25)).abs() < 1e-6);
    }
    // From arrival to sound: L + d_s (P113), and c(n) on request.
    assert!((latency.arrival_to_sound.as_secs_f64() - (1.75 + 0.25)).abs() < 1e-9);
    let plain_latency = rig.stream.speaker_latency(plain).expect("speaker");
    assert_eq!(plain_latency.receiver_latency_frames, None);
    assert!((plain_latency.arrival_to_sound.as_secs_f64() - 1.75).abs() < 1e-9);
    assert!((rig.c(44_099) - 1.0).abs() < 0.001, "c(n) for any n");

    // The first packet's sound time, reported per speaker (P115): P_s of
    // its first frame, about L − M after it is sent.
    for (id, delay) in [(reported, 0.25), (plain, 0.0)] {
        let (packet, _, at) = rig.first_sounds(id)[0];
        let first_frame = (packet * PACKET) as i64;
        assert!((at - (rig.c(first_frame) + rig.latency() + delay)).abs() < 1e-6);
        let latency = rig.stream.speaker_latency(id).expect("speaker");
        assert!((rig.clock.seconds(latency.first_sound) - at).abs() < 1e-6);
        let audio = rig.audio(id);
        let lead = at - audio[0].at;
        let expected = rig.latency() - audio[0].margin;
        assert!(lead <= expected && lead >= expected - 0.010, "{lead}");
    }

    // A receiver that plays as the sync packets ask buffers about the
    // latency asked for, less M: every packet arrives about L − M before
    // its first frame is heard, whatever the delay (P109).
    for id in [reported, plain] {
        let syncs = rig.syncs(id);
        for packet in rig.audio(id) {
            let (_, sync) = syncs
                .iter()
                .rev()
                .find(|(at, _)| *at <= packet.at)
                .expect("a sync before");
            let frames = packet.header.timestamp.wrapping_sub(sync.rtp_heard);
            let heard = stated(&rig, sync) + f64::from(frames) / 44_100.0;
            let lead = heard - packet.at;
            let expected = rig.latency() - packet.margin;
            assert!(
                lead <= expected + 0.001 && lead >= expected - 0.010,
                "lead {lead}"
            );
        }
    }
}

// covers: P86, P87, P88
#[test]
fn retransmission() {
    let mut rig = Rig::with_host();
    let id = rig.add(speaker(1, Codec::Alac));
    let other = rig.add(speaker(2, Codec::Alac));
    rig.run_for(Duration::from_secs(12));
    let originals: Vec<Vec<u8>> = rig
        .of(id)
        .into_iter()
        .filter(|s| s.transmit.kind == PacketKind::Audio)
        .map(|s| s.transmit.bytes.clone())
        .collect();
    assert!(originals.len() > 1_400);
    let seq_of = |bytes: &[u8]| u16::from_be_bytes([bytes[2], bytes[3]]);
    let last = seq_of(originals.last().expect("packet"));
    let request = |first: u16, count: u16| {
        ResendRequest {
            seq: 1,
            first,
            count,
        }
        .encode()
    };
    let replies = |rig: &mut Rig| -> Vec<Vec<u8>> {
        let start = rig.sent.len();
        rig.drain();
        rig.sent[start..]
            .iter()
            .filter(|s| s.transmit.kind == PacketKind::Resend)
            .map(|s| {
                assert_eq!(s.transmit.from, LocalSocket::Control);
                assert_eq!(s.transmit.to, destinations(1).control);
                s.transmit.bytes.clone()
            })
            .collect()
    };

    // Three missing packets: 0x80 0xD6, the packet's sequence number, then
    // the packet exactly as first sent (P86, P87).
    let first = last.wrapping_sub(2);
    rig.stream
        .handle_control_packet(id, &request(first, 3), rig.now);
    let got = replies(&mut rig);
    assert_eq!(got.len(), 3);
    for (offset, reply) in got.iter().enumerate() {
        let seq = first.wrapping_add(offset as u16);
        assert_eq!(
            &reply[..RESEND_PREFIX_BYTES],
            &[0x80, 0xD6, (seq >> 8) as u8, seq as u8]
        );
        let original = originals
            .iter()
            .find(|bytes| seq_of(bytes) == seq)
            .expect("sent");
        assert_eq!(&reply[RESEND_PREFIX_BYTES..], &original[..]);
    }

    // Packets beyond the last 1024 are skipped without a reply (P88).
    let old = last.wrapping_sub(1_100);
    rig.stream
        .handle_control_packet(id, &request(old, 1), rig.now);
    assert!(replies(&mut rig).is_empty());
    let counters = rig.stream.speaker_counters(id).expect("counters");
    assert_eq!(counters.not_held(), 1);
    let edge = last.wrapping_sub(1_023);
    rig.stream
        .handle_control_packet(id, &request(edge, 2), rig.now);
    assert_eq!(replies(&mut rig).len(), 2, "the last 1024 are held");

    // A request for more than 128 packets is clipped to the first 128.
    let start = last.wrapping_sub(199);
    rig.stream
        .handle_control_packet(id, &request(start, 200), rig.now);
    let got = replies(&mut rig);
    assert_eq!(got.len(), 128);
    assert_eq!(seq_of(&got[0][RESEND_PREFIX_BYTES..]), start);
    assert_eq!(
        seq_of(&got[127][RESEND_PREFIX_BYTES..]),
        start.wrapping_add(127)
    );

    // A packet is resent at most 3 times.
    let once = last.wrapping_sub(500);
    let mut counts = Vec::new();
    for _ in 0..4 {
        rig.stream
            .handle_control_packet(id, &request(once, 1), rig.now);
        counts.push(replies(&mut rig).len());
    }
    assert_eq!(counts, [1, 1, 1, 0]);

    // Other packets on the control socket are ignored.
    let sent = counters.resend_requests();
    rig.stream
        .handle_control_packet(id, &[0x80, 0xD4, 0, 0, 0, 0, 0, 0], rig.now);
    assert!(replies(&mut rig).is_empty());
    assert_eq!(counters.resend_requests(), sent);

    // One store serves every session; each keeps its own headers.
    let other_last = rig
        .audio(other)
        .last()
        .map(|a| a.header.seq)
        .expect("audio");
    rig.stream
        .handle_control_packet(other, &request(other_last, 1), rig.now);
    let start = rig.sent.len();
    rig.drain();
    let reply = rig.sent[start..]
        .iter()
        .find(|s| s.transmit.kind == PacketKind::Resend)
        .expect("reply")
        .transmit
        .bytes
        .clone();
    let mine = originals.last().expect("packet");
    assert_eq!(
        &reply[RESEND_PREFIX_BYTES + 12..],
        &mine[12..],
        "same payload"
    );
    assert_ne!(
        &reply[RESEND_PREFIX_BYTES..RESEND_PREFIX_BYTES + 12],
        &mine[..12]
    );
}

// covers: P121
#[test]
fn volume_software() {
    use common::{FakeReceiver, Link, config};

    let mut rig = Rig::with_host();
    let plain = rig.add(speaker(1, Codec::Alac));
    let mut sv = speaker(2, Codec::Alac);
    // −6.0206 dB is a gain of one half.
    sv.software_volume = Some(Volume::Db(20.0 * 0.5f64.log10()));
    let scaled = rig.add(sv);
    rig.run_for(Duration::from_secs(2));

    let plain_audio = rig.audio(plain);
    let scaled_audio = rig.audio(scaled);
    for packet in &scaled_audio {
        let j = packet_of(&rig, scaled, packet.header.timestamp);
        let same = plain_audio
            .iter()
            .find(|a| packet_of(&rig, plain, a.header.timestamp) == j)
            .expect("same packet");
        let expected: Vec<Frame> = decode(Codec::Alac, &same.payload)
            .iter()
            .map(|f| [half(f[0]), half(f[1])])
            .collect();
        assert_eq!(decode(Codec::Alac, &packet.payload), expected);
    }
    // Its own resend history serves the scaled packets.
    let last = scaled_audio.last().expect("audio");
    rig.stream.handle_control_packet(
        scaled,
        &ResendRequest {
            seq: 0,
            first: last.header.seq,
            count: 1,
        }
        .encode(),
        rig.now,
    );
    let start = rig.sent.len();
    rig.drain();
    let reply = &rig.sent[start..]
        .iter()
        .find(|s| s.transmit.kind == PacketKind::Resend)
        .expect("reply")
        .transmit
        .bytes;
    assert_eq!(&reply[RESEND_PREFIX_BYTES + 12..], &last.payload[..]);

    // Muted: zeros from the next packets cut.
    rig.stream.set_software_volume(scaled, Volume::Muted);
    let before = rig.audio(scaled).len();
    rig.run_for(Duration::from_millis(300));
    let muted = rig.audio(scaled);
    for packet in &muted[before + 2..] {
        assert!(
            decode(Codec::Alac, &packet.payload)
                .iter()
                .all(|f| *f == [0, 0])
        );
    }
    assert!(
        !rig.stream.set_software_volume(plain, Volume::Muted),
        "plain speaker"
    );

    // A speaker that joins with a delay gets its encoding of packets cut
    // before it joined.
    let mut late = speaker(3, Codec::Alac);
    late.software_volume = Some(Volume::Db(0.0));
    late.delay = Duration::from_secs(1);
    let joined = rig.add(late);
    rig.run_for(Duration::from_millis(100));
    let packet = &rig.audio(joined)[0];
    let n = rig.frame_of(joined, packet.header.timestamp) as u64;
    assert!(n < rig.stream.next_packet() * PACKET);
    assert_eq!(decode(Codec::Alac, &packet.payload)[0], host_frame(n));

    // And its session sends no volume requests (P121).
    let mut session_config = config();
    session_config.software_volume = true;
    let mut link = Link::new(session_config, FakeReceiver::new());
    link.connect();
    assert_eq!(link.keys(), ["OPTIONS", "ANNOUNCE", "SETUP", "RECORD"]);
    link.session.set_volume(Volume::Db(-3.0), link.now);
    link.advance(Duration::from_secs(1));
    assert!(!link.keys().contains(&"SET_PARAMETER".to_owned()));

    // `sm=true` alone: the samples are untouched at any level and zero
    // while muted; mute never goes out as −144, levels still do.
    let mut sm = speaker(4, Codec::Alac);
    sm.software_mute = true;
    let mute_only = rig.add(sm);
    assert!(rig.stream.set_software_volume(mute_only, Volume::Db(-20.0)));
    rig.run_for(Duration::from_millis(300));
    let packet = rig.audio(mute_only).last().cloned().expect("audio");
    let n = rig.frame_of(mute_only, packet.header.timestamp) as u64;
    assert_eq!(
        decode(Codec::Alac, &packet.payload)[0],
        host_frame(n),
        "untouched at a level"
    );
    rig.stream.set_software_volume(mute_only, Volume::Muted);
    let before = rig.audio(mute_only).len();
    rig.run_for(Duration::from_millis(300));
    for packet in &rig.audio(mute_only)[before + 2..] {
        assert!(
            decode(Codec::Alac, &packet.payload)
                .iter()
                .all(|f| *f == [0, 0])
        );
    }
    let mut session_config = config();
    session_config.software_mute = true;
    session_config.initial_volume = Volume::Muted;
    let mut link = Link::new(session_config, FakeReceiver::new());
    link.connect();
    assert_eq!(
        link.keys(),
        ["OPTIONS", "ANNOUNCE", "SETUP", "RECORD"],
        "no −144 after RECORD"
    );
    link.session.set_volume(Volume::Db(-10.0), link.now);
    link.pump();
    assert_eq!(link.last("SET_PARAMETER").body, b"volume: -10.000000\r\n");
    link.advance(Duration::from_secs(1));
    link.session.set_volume(Volume::Muted, link.now);
    link.advance(Duration::from_secs(1));
    let volume_requests = link.keys().iter().filter(|k| *k == "SET_PARAMETER").count();
    assert_eq!(volume_requests, 1, "mute stays in the samples");
}

fn half(sample: i16) -> i16 {
    (f64::from(sample) * 0.5).round() as i16
}

// covers: P143
#[test]
fn restart_timeline() {
    let mut rig = Rig::with_host();
    let playing = rig.add(speaker(1, Codec::Alac));
    let mut config = speaker(2, Codec::Alac);
    config.delay = Duration::from_millis(400);
    let delayed = rig.add(config);
    rig.run_for(Duration::from_secs(5));
    let before: Vec<_> = [playing, delayed]
        .iter()
        .map(|&id| rig.audio(id).last().expect("audio").header)
        .collect();

    // The host restarts its capture at 5 s; its new frames count from 0.
    let restart_at = rig.t();
    let pending_before = rig.writer.position() as i64;
    rig.stream.restart(rig.now);
    rig.drain();
    let mut host = Host::new(restart_at);
    host.block = 512;
    rig.host = Some(host);
    rig.run_for(Duration::from_secs(3));

    // Every session is flushed, numbering going on.
    for (index, id) in [playing, delayed].into_iter().enumerate() {
        let flush = flushes(&rig, id);
        assert_eq!(flush.len(), 1);
        assert_eq!(flush[0].0, before[index].seq.wrapping_add(1));
        assert_eq!(flush[0].1, before[index].timestamp.wrapping_add(352));
    }
    // The frame count and the model start over; the stream reports the new
    // mapping.
    assert!(rig.events.iter().any(|event| matches!(
        event,
        StreamEvent::Timeline { restarted: true, origin, .. } if *origin == pending_before
    )));
    assert_eq!(rig.stream.origin(), pending_before);
    assert!((rig.c(0) - (restart_at + 1.0 / 44_100.0)).abs() < 1e-6);
    let (packet, rtptime, _) = *rig.first_sounds(playing).last().expect("report");
    assert_eq!(packet, 0, "the new timeline starts at frame 0");
    assert_eq!(rtptime, before[0].timestamp.wrapping_add(352));
    // They come back in step, with the new frames at their places.
    for (id, delay) in [(playing, 0.0), (delayed, 0.4)] {
        let syncs = rig.syncs(id);
        let after: Vec<_> = syncs.iter().filter(|(at, _)| *at >= restart_at).collect();
        assert!(after[0].1.first);
        for (_, sync) in &after {
            let n = rig.frame_of(id, sync.rtp_heard);
            assert!((stated(&rig, sync) - (rig.c(n) + rig.latency() + delay)).abs() < 1e-6);
        }
        for packet in rig.audio(id).iter().filter(|a| a.at > restart_at + 0.5) {
            let n = rig.frame_of(id, packet.header.timestamp) as u64;
            assert_eq!(decode(Codec::Alac, &packet.payload)[0], host_frame(n));
        }
    }
    assert_eq!(rig.stream.counters().silent_frames(), 0);
}

// covers: P144
#[test]
fn diagnostics_counters() {
    let mut rig = Rig::with_host();
    rig.host.as_mut().expect("host").stalls = vec![(150, 0.050)];
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(3));
    let counters = rig.stream.speaker_counters(id).expect("counters");
    let stream = rig.stream.counters();
    // Reads come from another thread, without locks.
    let reader = {
        let counters = counters.clone();
        let stream = stream.clone();
        std::thread::spawn(move || (counters.packets_sent(), stream.rate()))
    };
    let (sent, rate) = reader.join().expect("reader");
    assert_eq!(sent, rig.audio(id).len() as u64);
    assert!((rate - 44_100.0).abs() < 44.1, "R̂ {rate}");
    assert!(counters.silent_frames() > 0);
    assert_eq!(counters.silent_frames(), stream.silent_frames());
    assert!(counters.input_frames_dropped() > 0);
    assert_eq!(
        counters.input_frames_dropped(),
        stream.input_frames_dropped()
    );

    let last = rig.audio(id).last().expect("audio").header.seq;
    rig.stream.handle_control_packet(
        id,
        &ResendRequest {
            seq: 0,
            first: last.wrapping_sub(1),
            count: 2,
        }
        .encode(),
        rig.now,
    );
    rig.stream.handle_control_packet(
        id,
        &ResendRequest {
            seq: 0,
            first: last.wrapping_add(10),
            count: 1,
        }
        .encode(),
        rig.now,
    );
    assert_eq!(counters.resend_requests(), 2);
    assert_eq!(counters.packets_resent(), 2);
    assert_eq!(counters.not_held(), 1);
    let timing = audiosync_airplay::rtp::TimingPacket {
        reply: false,
        seq: 3,
        origin: audiosync_airplay::clock::NtpTime(0),
        receive: audiosync_airplay::clock::NtpTime(0),
        transmit: audiosync_airplay::clock::NtpTime(1),
    }
    .encode();
    let source = destinations(1).audio;
    rig.stream
        .answer_timing(id, &timing, source, rig.now, rig.now)
        .expect("reply");
    assert_eq!(counters.timing_replies(), 1);
    // A driver that wakes 50 ms late sends the overdue packets at once and
    // counts them late.
    assert_eq!(counters.late_packets(), 0);
    let late_now = rig.now + Duration::from_millis(50);
    rig.host = None;
    rig.jump_to(late_now);
    assert!(counters.late_packets() >= 5);
    assert_eq!(stream.packets_encoded(), rig.stream.next_packet());
}

#[test]
fn speakers_never_share_an_id_and_removal_stops_their_packets() {
    let mut rig = Rig::with_host();
    let first = rig.add(speaker(1, Codec::Alac));
    let second = rig.add(speaker(2, Codec::Alac));
    assert_ne!(first, second);
    rig.run_for(Duration::from_millis(500));
    assert!(rig.stream.remove_speaker(first));
    assert!(!rig.stream.remove_speaker(first));
    let count = rig.of(first).len();
    rig.run_for(Duration::from_millis(500));
    assert_eq!(rig.of(first).len(), count);
    assert!(rig.stream.speaker_latency(first).is_none());
    let third = rig.add(speaker(3, Codec::Alac));
    assert_ne!(third, first);
}
