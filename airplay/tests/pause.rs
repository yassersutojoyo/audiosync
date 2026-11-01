//! An explicit pause (P213): the stream against fake receivers on a fake
//! clock, then sessions over loopback sockets.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use audiosync_airplay::audio::FRAMES_PER_PACKET;
use audiosync_airplay::discovery::DeviceKey;
use audiosync_airplay::error::Warning;
use audiosync_airplay::metadata::Artwork;
use audiosync_airplay::rtp::ResendRequest;
use audiosync_airplay::rtsp::Method;
use audiosync_airplay::sdp::Codec;
use audiosync_airplay::sender::{
    NowPlaying, Sender, SenderConfig, SenderEvent, SpeakerEvent, SpeakerOptions, SpeakerState,
};
use audiosync_airplay::stream::{PacketKind, SpeakerCounters, SpeakerId, StreamEvent};
use audiosync_airplay::volume::Volume;
use common::net::{Behavior, Events, HostAudio, NetReceiver, serial, wait_until};
use common::rig::{Rig, decode, host_frame, speaker};

const PACKET: u64 = FRAMES_PER_PACKET as u64;

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

fn all_flushes(rig: &Rig) -> usize {
    rig.events
        .iter()
        .filter(|event| matches!(event, StreamEvent::Flush { .. }))
        .count()
}

/// Audio and sync datagrams to a speaker sent in [from, to).
fn media_between(rig: &Rig, id: SpeakerId, from: f64, to: f64) -> usize {
    rig.of(id)
        .into_iter()
        .filter(|sent| sent.at >= from && sent.at < to)
        .filter(|sent| matches!(sent.transmit.kind, PacketKind::Audio | PacketKind::Sync))
        .count()
}

// covers: P213
#[test]
fn pause_flushes_and_resumes() {
    let mut rig = Rig::with_host();
    let near = rig.add(speaker(1, Codec::Pcm));
    let mut late = speaker(2, Codec::Pcm);
    late.delay = Duration::from_millis(300);
    let late = rig.add(late);
    rig.run_for(Duration::from_secs(3));

    let paused_at = rig.t();
    assert!(rig.stream.pause(rig.now));
    rig.drain();
    for id in [near, late] {
        let flush = flushes(&rig, id);
        assert_eq!(flush.len(), 1, "one FLUSH per session");
        // The flush point is the first packet not sent.
        let last = rig
            .audio(id)
            .last()
            .cloned()
            .expect("audio before the pause");
        assert_eq!(flush[0].0, last.header.seq.wrapping_add(1));
        assert_eq!(flush[0].1, last.header.timestamp.wrapping_add(352));
    }

    // While paused nothing goes out, a second pause does nothing, delay and
    // Lf changes send no FLUSH, and resends of earlier packets get no answer.
    let near_seq = flushes(&rig, near)[0].0;
    assert!(!rig.stream.pause(rig.now));
    rig.stream
        .set_delay(near, Duration::from_millis(100), rig.now);
    rig.stream.set_latency(44_100, rig.now);
    rig.run_for(Duration::from_secs(2));
    let request = ResendRequest {
        seq: 1,
        first: near_seq.wrapping_sub(4),
        count: 4,
    };
    rig.stream
        .handle_control_packet(near, &request.encode(), rig.now);
    rig.run_for(Duration::from_millis(500));
    for id in [near, late] {
        assert_eq!(media_between(&rig, id, paused_at, rig.t() + 1.0), 0);
        assert_eq!(flushes(&rig, id).len(), 1, "no FLUSH while paused");
    }
    assert!(
        rig.of(near)
            .iter()
            .all(|sent| sent.transmit.kind != PacketKind::Resend)
    );

    for id in [near, late] {
        rig.stream.flush_answered(id);
    }
    let resumed_at = rig.t();
    let first_frame = rig.writer.position() as i64 - rig.stream.origin();
    assert!(rig.stream.resume(rig.now));
    assert!(!rig.stream.resume(rig.now), "a resume while playing");
    rig.run_for(Duration::from_secs(3));
    assert_eq!(all_flushes(&rig), 2, "a resume sends no FLUSH");

    // Both continue from their flush points with the first whole packet at
    // or after the first frame delivered after the resume.
    let packet = (first_frame as u64).div_ceil(PACKET);
    for id in [near, late] {
        let (seq, rtptime) = flushes(&rig, id)[0];
        let audio = rig.audio(id);
        let index = audio
            .iter()
            .position(|a| a.at >= resumed_at)
            .expect("audio after the resume");
        let first = &audio[index];
        assert_eq!(first.header.seq, seq);
        assert_eq!(first.header.timestamp, rtptime);
        assert!(first.header.marker);
        assert!(audio[index + 1..].iter().all(|a| !a.header.marker));
        for pair in audio[index..].windows(2) {
            assert_eq!(pair[1].header.seq, pair[0].header.seq.wrapping_add(1));
        }
        let (resumed_packet, resumed_rtp, _) = *rig.first_sounds(id).last().unwrap();
        assert_eq!(resumed_packet, packet);
        assert_eq!(resumed_rtp, rtptime);
        let frames = decode(Codec::Pcm, &first.payload);
        let ring = packet as i64 * PACKET as i64 + rig.stream.origin();
        assert_eq!(frames[0], host_frame(ring as u64));
        let syncs = rig.syncs(id);
        let sync = syncs
            .iter()
            .find(|(at, _)| *at >= resumed_at)
            .expect("a sync after the resume");
        assert!(sync.1.first);
        assert_eq!(sync.1.next_rtp, rtptime);
    }
    // The delay set during the pause applies at the resume, and both
    // speakers play the resumed frame at its time under P96.
    let near_sound = rig.first_sounds(near).last().unwrap().2;
    let late_sound = rig.first_sounds(late).last().unwrap().2;
    assert!((late_sound - near_sound - 0.2).abs() < 1e-6);
    assert_eq!(rig.stream.latency_frames(), 44_100);
}

// Part of P213: a resume before a session's FLUSH answer waits for it.
// covers: P213
#[test]
fn resume_waits_for_the_flush_answer() {
    let mut rig = Rig::with_host();
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(2));
    rig.stream.pause(rig.now);
    rig.run_for(Duration::from_millis(50));
    let resumed_at = rig.t();
    rig.stream.resume(rig.now);
    rig.run_for(Duration::from_secs(1));
    assert_eq!(media_between(&rig, id, resumed_at, rig.t() + 1.0), 0);

    rig.stream.flush_answered(id);
    rig.run_for(Duration::from_secs(1));
    let (seq, rtptime) = flushes(&rig, id)[0];
    let audio = rig.audio(id);
    let first = audio
        .iter()
        .find(|a| a.at >= resumed_at)
        .expect("audio once answered");
    assert_eq!((first.header.seq, first.header.timestamp), (seq, rtptime));
    assert!(first.header.marker);
}

// Part of P213 and P85: silence never pauses.
// covers: P85, P213
#[test]
fn silence_sends_no_flush() {
    let mut rig = Rig::new(Default::default());
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(5));
    let audio = rig.audio(id);
    assert!(audio.len() > 500, "silence keeps the packets flowing");
    assert_eq!(all_flushes(&rig), 0);

    let mut rig = Rig::with_host();
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_secs(2));
    rig.host = None;
    let underrun = rig.t();
    rig.run_for(Duration::from_secs(3));
    assert_eq!(all_flushes(&rig), 0, "an underrun is no pause");
    assert!(rig.audio(id).iter().filter(|a| a.at >= underrun).count() > 300);
}

// Part of P213: with nothing sent there is nothing to flush.
#[test]
fn pause_while_stopped_sends_nothing() {
    let mut rig = Rig::with_host();
    rig.run_for(Duration::from_secs(1));
    assert!(rig.stream.pause(rig.now));
    rig.run_for(Duration::from_secs(1));
    assert!(rig.events.is_empty() && rig.sent.is_empty());
    assert!(rig.stream.resume(rig.now));
    rig.run_for(Duration::from_secs(1));
    assert_eq!(all_flushes(&rig), 0);
}

/// A receiver that takes PCM only.
const PCM_TXT: &[&str] = &[
    "txtvers=1",
    "ch=2",
    "cn=0",
    "et=0",
    "sr=44100",
    "ss=16",
    "tp=UDP",
    "md=0,1,2",
];

const LONG: Duration = Duration::from_secs(15);

// Part of P213, over loopback: the host's pause reaches the receiver as
// one FLUSH and the audio stops; the resume continues at its seq.
// covers: P213
#[test]
fn a_host_pause_flushes_the_receiver_over_loopback() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, writer) = Sender::start(SenderConfig::default()).expect("the sender starts");
    let _host = HostAudio::start(writer);
    let mut events = Events::default();
    let device = receiver.device(1, PCM_TXT);
    let key = device.key.clone();
    sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some()
    );
    assert!(wait_until(LONG, || receiver.log().audio.len() >= 100));

    sender.pause();
    assert!(wait_until(LONG, || receiver.log().count("FLUSH") == 1));
    let seq = {
        let log = receiver.log();
        let flush = log.last("FLUSH").expect("FLUSH");
        let info = flush.headers.get("RTP-Info").expect("RTP-Info").to_owned();
        let seq = info
            .split(';')
            .find_map(|part| part.strip_prefix("seq="))
            .and_then(|value| value.parse::<u16>().ok())
            .expect("seq");
        assert_eq!(
            log.audio.last().map(|a| a.header.seq.wrapping_add(1)),
            Some(seq)
        );
        seq
    };
    std::thread::sleep(Duration::from_millis(300));
    let held = receiver.log().audio.len();
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(receiver.log().audio.len(), held, "no audio while paused");

    sender.resume();
    assert!(wait_until(LONG, || receiver.log().audio.len() > held + 50));
    let log = receiver.log();
    assert_eq!(log.count("FLUSH"), 1, "a resume sends no request");
    let first = &log.audio[held];
    assert_eq!(first.header.seq, seq);
    assert!(first.header.marker);
    sender.shutdown();
}

/// A sender playing to `receiver`, once audio flows.
fn play(receiver: &NetReceiver, id: u8) -> (Sender, HostAudio, Events, DeviceKey) {
    let (sender, writer) = Sender::start(SenderConfig::default()).expect("the sender starts");
    let host = HostAudio::start(writer);
    let mut events = Events::default();
    let device = receiver.device(id, PCM_TXT);
    let key = device.key.clone();
    sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some()
    );
    assert!(wait_until(LONG, || receiver.log().audio.len() >= 50));
    (sender, host, events, key)
}

/// The `seq` the `nth` (from 0) FLUSH request names.
fn flush_seq(receiver: &NetReceiver, nth: usize) -> u16 {
    rtp_info_seq(receiver, Method::Flush, nth)
}

/// The `seq` in the RTP-Info of the `nth` (from 0) `method` request.
fn rtp_info_seq(receiver: &NetReceiver, method: Method, nth: usize) -> u16 {
    let log = receiver.log();
    let request = log
        .requests
        .iter()
        .filter(|(_, request)| request.method == method)
        .nth(nth)
        .map(|(_, request)| request.clone())
        .expect("request");
    let info = request
        .headers
        .get("RTP-Info")
        .expect("RTP-Info")
        .to_owned();
    info.split(';')
        .find_map(|part| part.strip_prefix("seq="))
        .and_then(|value| value.parse().ok())
        .expect("seq")
}

/// The first audio packet the receiver got after `at`.
fn first_audio_after(receiver: &NetReceiver, at: Instant) -> Option<(Instant, u16, bool)> {
    receiver
        .log()
        .audio
        .iter()
        .find(|packet| packet.at > at)
        .map(|packet| (packet.at, packet.header.seq, packet.header.marker))
}

// Part of P213: a delay FLUSH still unanswered when the pause comes doesn't
// end the resume's wait; only the pause FLUSH's answer does.
// covers: P213
#[test]
fn a_resume_waits_for_the_pause_flush_behind_a_delay_flush() {
    const ANSWER: Duration = Duration::from_millis(800);
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, _host, _events, key) = play(&receiver, 1);
    receiver
        .behavior()
        .answer_delay
        .insert("FLUSH".to_owned(), ANSWER);
    sender.set_delay(&key, Duration::from_millis(200));
    assert!(wait_until(LONG, || receiver.log().count("FLUSH") == 1));
    sender.pause();
    let paused = Instant::now();
    sender.resume();
    assert!(wait_until(LONG, || receiver.log().count("FLUSH") == 2));
    // The pause FLUSH names the first packet held back. Packets before it
    // left before the sender took the pause, however late a busy runner
    // stamps them, so they must beat that FLUSH to the receiver.
    let held = flush_seq(&receiver, 1);
    let first_held = || {
        receiver
            .log()
            .audio
            .iter()
            .find(|packet| !before(packet.header.seq, held))
            .map(|packet| (packet.at, packet.header.seq, packet.header.marker))
    };
    assert!(wait_until(LONG, || first_held().is_some()));
    let (at, seq, marker) = first_held().expect("audio");
    let log = receiver.log();
    let flushed = log
        .requests
        .iter()
        .filter(|(_, request)| request.method == Method::Flush)
        .nth(1)
        .map(|(at, _)| *at)
        .expect("FLUSH");
    assert!(
        log.audio
            .iter()
            .filter(|packet| before(packet.header.seq, held))
            .all(|packet| packet.at < flushed)
    );
    // The delay FLUSH is answered about ANSWER after the pause, the pause
    // FLUSH about ANSWER after that.
    assert!(
        at >= paused + ANSWER + ANSWER / 2,
        "audio {:?} after the pause",
        at - paused
    );
    assert_eq!(seq, held);
    assert!(marker);
    sender.shutdown();
}

// Part of P100: a delay change made while artwork is in flight applies when
// its FLUSH goes out, so no packet under the new delay reaches the receiver
// before that FLUSH, which a receiver emptying its buffer would drop.
// covers: P100
#[test]
fn a_delay_change_behind_artwork_sends_nothing_new_before_its_flush() {
    const ARTWORK: Duration = Duration::from_millis(1500);
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, _host, _events, key) = play(&receiver, 1);
    receiver
        .behavior()
        .answer_delay
        .insert("SET_PARAMETER".to_owned(), ARTWORK);
    sender
        .set_now_playing(NowPlaying {
            artwork: Some(Artwork::single(vec![
                0xFF, 0xD8, 0xFF, 0xE0, 0, 0, 0xFF, 0xD9,
            ])),
            ..NowPlaying::default()
        })
        .expect("valid artwork");
    sender.set_delay(&key, Duration::from_millis(200));
    assert!(wait_until(LONG, || receiver.log().count("FLUSH") == 1));
    let held = flush_seq(&receiver, 0);
    assert!(wait_until(LONG, || {
        receiver
            .log()
            .audio
            .iter()
            .any(|packet| !before(packet.header.seq, held))
    }));
    let log = receiver.log();
    let at = |method: Method, kind: Option<&str>| {
        log.requests
            .iter()
            .find(|(_, request)| {
                request.method == method
                    && kind.is_none_or(|kind| request.headers.get("Content-Type") == Some(kind))
            })
            .map(|(at, _)| *at)
            .expect("request")
    };
    let flushed = at(Method::Flush, None);
    assert!(flushed >= at(Method::SetParameter, Some("image/jpeg")) + ARTWORK);
    let early: Vec<u16> = log
        .audio
        .iter()
        .filter(|packet| !before(packet.header.seq, held) && packet.at < flushed)
        .map(|packet| packet.header.seq)
        .collect();
    assert!(
        early.is_empty(),
        "before the FLUSH naming {held}: {early:?}"
    );
    sender.shutdown();
}

/// Whether RTP `seq` comes before `next` in sequence order.
fn before(seq: u16, next: u16) -> bool {
    (next.wrapping_sub(seq) as i16) > 0
}

// Part of P213: a refused pause FLUSH still ends the wait, with a warning.
#[test]
fn a_refused_pause_flush_lets_the_resume_play() {
    let _serial = serial();
    let mut behavior = Behavior::default();
    behavior.status.insert("FLUSH".to_owned(), 500);
    let receiver = NetReceiver::start(behavior);
    let (sender, _host, mut events, key) = play(&receiver, 1);
    sender.pause();
    assert!(wait_until(LONG, || receiver.log().count("FLUSH") == 1));
    let refused = events.wait_for(&sender, LONG, |event| {
        matches!(
            event,
            SenderEvent::Speaker {
                event: SpeakerEvent::Warning(Warning::RequestFailed {
                    method: Method::Flush,
                    ..
                }),
                ..
            }
        )
    });
    assert!(refused.is_some(), "{:?}", events.of(&key));
    let resumed = Instant::now();
    sender.resume();
    assert!(wait_until(LONG, || first_audio_after(&receiver, resumed).is_some()));
    let (_, seq, marker) = first_audio_after(&receiver, resumed).expect("audio");
    assert_eq!(seq, flush_seq(&receiver, 0));
    assert!(marker);
    sender.shutdown();
}

// Part of P213 and P142: a speaker added, or one that reconnects, while the
// stream is paused sends nothing until the resume.
#[test]
fn a_speaker_added_or_reconnected_while_paused_waits_for_the_resume() {
    let _serial = serial();
    let first = NetReceiver::start(Behavior::default());
    let second = NetReceiver::start(Behavior::default());
    let (sender, _host, mut events, first_key) = play(&first, 1);
    sender.pause();
    assert!(wait_until(LONG, || first.log().count("FLUSH") == 1));

    let device = second.device(2, PCM_TXT);
    let second_key = device.key.clone();
    sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
    first.kill();
    for (key, nth) in [(&second_key, 1), (&first_key, 2)] {
        assert!(
            events
                .wait_state(&sender, key, nth, LONG, |s| *s == SpeakerState::Playing)
                .is_some(),
            "{:?}",
            events.of(key)
        );
    }
    std::thread::sleep(Duration::from_millis(500));
    let held = flush_seq(&first, 0);
    let reconnected = rtp_info_seq(&first, Method::Record, 1);
    {
        let log = first.log();
        let source = log.audio.first().expect("audio").from;
        // The pause FLUSH names the first packet held back, so the first
        // session sent only the run before it, however late a busy runner
        // stamps it; the reconnected one sent nothing.
        let sent: Vec<u16> = log
            .audio
            .iter()
            .filter(|packet| packet.from == source)
            .map(|packet| packet.header.seq)
            .collect();
        assert!(
            sent.windows(2)
                .all(|pair| pair[1] == pair[0].wrapping_add(1)),
            "{sent:?}"
        );
        assert_eq!(sent.last().map(|seq| seq.wrapping_add(1)), Some(held));
        assert_eq!(sent.len(), log.audio.len());
    }
    assert!(second.log().audio.is_empty());

    let resumed = Instant::now();
    sender.resume();
    for receiver in [&first, &second] {
        assert!(wait_until(LONG, || first_audio_after(receiver, resumed).is_some()));
        let (_, _, marker) = first_audio_after(receiver, resumed).expect("audio");
        assert!(marker, "the first packet of a session");
    }
    let (_, seq, _) = first_audio_after(&first, resumed).expect("audio");
    assert_eq!(seq, reconnected, "the reconnected session's first packet");
    sender.shutdown();
}

// Part of P213, P91, P118 and P131: while paused the session stays up, its
// timing requests are answered and a volume change goes out.
// covers: P213
#[test]
fn timing_and_volume_go_on_while_paused() {
    let _serial = serial();
    let receiver = NetReceiver::start(Behavior::default());
    let (sender, writer) = Sender::start(SenderConfig::default()).expect("the sender starts");
    let _host = HostAudio::start(writer);
    let mut events = Events::default();
    let device = receiver.device(1, PCM_TXT);
    let key = device.key.clone();
    let counters: Arc<SpeakerCounters> =
        sender.add_speaker(device, SpeakerOptions::new(Volume::Db(-20.0)));
    assert!(
        events
            .wait_state(&sender, &key, 1, LONG, |s| *s == SpeakerState::Playing)
            .is_some()
    );
    assert!(wait_until(LONG, || receiver.log().audio.len() >= 50));
    sender.pause();
    assert!(wait_until(LONG, || receiver.log().count("FLUSH") == 1));
    let replies = counters.timing_replies();
    sender.set_volume(&key, Volume::Db(-10.0));
    assert!(wait_until(LONG, || receiver
        .log()
        .requests
        .iter()
        .any(|(_, request)| request.method == Method::SetParameter
            && request.body == b"volume: -10.000000\r\n")));
    assert!(wait_until(LONG, || counters.timing_replies() >= replies + 3));
    events.drain(&sender);
    assert!(
        !events
            .of(&key)
            .iter()
            .any(|event| matches!(event, SpeakerEvent::Error(_))),
        "{:?}",
        events.of(&key)
    );
    assert_eq!(
        events.of(&key).iter().rev().find_map(|event| match event {
            SpeakerEvent::State(state) => Some(*state),
            _ => None,
        }),
        Some(SpeakerState::Playing)
    );
    sender.shutdown();
}
