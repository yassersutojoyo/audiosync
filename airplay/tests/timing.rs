//! The sender clock and the AirPlay 1 timing exchange (P89 to P92), with
//! the session's liveness rule on the same requests (P131).

mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use audiosync_airplay::clock::{NTP_ORIGIN_SECONDS, NtpTime, SenderClock};
use audiosync_airplay::rtp::{SyncPacket, TimingPacket, timing_reply};
use audiosync_airplay::sdp::Codec;
use audiosync_airplay::session::State;
use audiosync_airplay::stream::{LocalSocket, PacketKind};
use common::rig::{Rig, speaker};
use common::{FakeReceiver, Link, config};

// covers: P89, P90, P91, P92
#[test]
fn timing_reply_layout_and_the_sender_clock() {
    // The sender clock reads as NTP format from 0x83AA7E80 at its origin
    // (P89), on the monotonic clock.
    let origin = Instant::now();
    let clock = SenderClock::new(origin);
    assert_eq!(clock.ntp(origin), NtpTime(NTP_ORIGIN_SECONDS << 32));
    let later = clock.ntp(origin + Duration::from_millis(1500));
    assert_eq!(later.seconds(), 0x83AA_7E81);
    assert_eq!(later.fraction(), 0x8000_0000, "half a second");
    assert!(clock.ntp(origin + Duration::from_micros(1)) > clock.ntp(origin));
    assert!((clock.seconds_of(later) - 1.5).abs() < 1e-9);

    // A request: 0x80 0xD2, a sequence number, four zero bytes, then
    // origin, receive and transmit, only transmit filled in (P90).
    let request = TimingPacket {
        reply: false,
        seq: 7,
        origin: NtpTime(0),
        receive: NtpTime(0),
        transmit: NtpTime(0xE1D2_C3B4_A596_8778),
    }
    .encode();
    assert_eq!(&request[..8], &[0x80, 0xD2, 0x00, 0x07, 0, 0, 0, 0]);

    // The reply (P91): 0x80 0xD3, the request's sequence number, four zero
    // bytes, origin = the request's transmit time unchanged, receive = when
    // it arrived, transmit = just before sending.
    let received = clock.ntp(origin + Duration::from_millis(20));
    let sent = clock.ntp(origin + Duration::from_micros(20_150));
    let reply = timing_reply(&request, received, sent).expect("a request");
    assert_eq!(reply.len(), 32);
    assert_eq!(&reply[..8], &[0x80, 0xD3, 0x00, 0x07, 0, 0, 0, 0]);
    let parsed = TimingPacket::parse(&reply).expect("reply");
    assert!(parsed.reply);
    assert_eq!(parsed.origin, NtpTime(0xE1D2_C3B4_A596_8778));
    assert_eq!(parsed.receive, received);
    assert_eq!(parsed.transmit, sent);
    assert!(parsed.receive < parsed.transmit, "receive before transmit");

    // Anything else on the timing socket is ignored (P92).
    assert_eq!(timing_reply(&reply, received, sent), None, "a reply");
    assert_eq!(timing_reply(&request[..31], received, sent), None, "short");
    let sync = SyncPacket {
        first: true,
        seq: 0,
        rtp_heard: 1,
        time: received,
        next_rtp: 2,
    }
    .encode();
    assert_eq!(timing_reply(&sync, received, sent), None, "a sync packet");
    let mut wrong_version = request;
    wrong_version[0] = 0x40;
    assert_eq!(timing_reply(&wrong_version, received, sent), None);

    // Through the stream: the reply leaves the timing socket for the
    // request's source, even with `timing_port=0` from SETUP, and is
    // counted; the stream itself never sends a timing request (P92).
    let mut receiver = FakeReceiver::new();
    receiver.transport =
        "RTP/AVP/UDP;unicast;mode=record;server_port=50000;control_port=50001;timing_port=0"
            .to_owned();
    let mut link = Link::new(config(), receiver);
    link.connect();
    assert_eq!(link.session.state(), State::Playing);
    assert_eq!(
        link.session.receiver_ports().map(|p| p.timing_port),
        Some(0)
    );
    let mut rig = Rig::with_host();
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(1));
    let source = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 0, 2, 21)), 6_002);
    let at = rig.now;
    let transmit = rig
        .stream
        .answer_timing(id, &request, source, at, at + Duration::from_micros(80))
        .expect("answered");
    assert_eq!(transmit.to, source);
    assert_eq!(transmit.from, LocalSocket::Timing);
    assert_eq!(transmit.kind, PacketKind::Timing);
    let parsed = TimingPacket::parse(&transmit.bytes).expect("reply");
    assert_eq!(parsed.receive, rig.clock.ntp(at));
    assert_eq!(
        rig.stream.speaker_counters(id).map(|c| c.timing_replies()),
        Some(1)
    );
    assert!(
        rig.stream
            .answer_timing(id, &sync, source, at, at)
            .is_none()
    );
    assert!(
        rig.sent
            .iter()
            .all(|sent| matches!(sent.transmit.kind, PacketKind::Audio | PacketKind::Sync)),
        "no timing requests of its own"
    );
}
