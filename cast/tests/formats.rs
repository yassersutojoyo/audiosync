//! The two stream formats (C63), what the sender adds to the latency
//! (C72), and the FLAC fallback of a session (C84). FLAC is checked by
//! decoding it with an independent decoder.

mod common;

use std::io::Cursor;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use audiosync_cast::audio::{AudioWriter, Codec, StreamFormat, audio_ring};
use audiosync_cast::media::{IdleReason, MediaError, Metadata};
use audiosync_cast::message::NS_MEDIA;
use audiosync_cast::session::{
    CastSession, SessionConfig, SessionError, SessionEvent, falls_back_to_flac,
};
use audiosync_cast::stream::{QUEUE_LIMIT, StreamConfig, StreamServer};
use common::{Behavior, FakeDevice, OnLoad, http};
use serde_json::json;

const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

struct Feed {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Drop for Feed {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// 10 ms of a stepped ramp every 10 ms, so the decoded samples can be told
/// apart from silence and from each other.
fn feed(writer: AudioWriter, rate: u32) -> Feed {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let thread = thread::spawn(move || {
        let frames = rate as usize / 100;
        let start = Instant::now();
        let mut written = 0u32;
        let mut value: i16 = 1;
        while !flag.load(Ordering::SeqCst) {
            let mut block = Vec::with_capacity(frames * 2);
            for _ in 0..frames {
                value = value.wrapping_add(37);
                block.push(value);
                block.push(value.wrapping_neg());
            }
            writer.write_i16(&block);
            written += 1;
            let next = start + Duration::from_millis(10) * written;
            thread::sleep(next.saturating_duration_since(Instant::now()));
        }
    });
    Feed {
        stop,
        thread: Some(thread),
    }
}

/// The samples of a FLAC stream up to the first frame that is cut off,
/// with the stream's STREAMINFO.
fn decode(bytes: &[u8]) -> (claxon::metadata::StreamInfo, Vec<i32>) {
    let mut reader = claxon::FlacReader::new(Cursor::new(bytes.to_vec())).expect("a FLAC stream");
    let info = reader.streaminfo();
    let samples = reader.samples().map_while(Result::ok).collect();
    (info, samples)
}

fn pcm(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

// covers: C63
#[test]
fn flac_is_a_native_stream_with_an_unknown_length_that_decodes_to_the_input() {
    for rate in [44_100, 48_000] {
        let format = StreamFormat::flac(rate).expect("rate");
        assert_eq!(format.content_type(), "audio/flac");
        assert_eq!(format.codec(), Codec::Flac);
        let block_frames = format.block_frames();
        let mut encoder = format.encoder().expect("encoder");
        let mut stream = format.stream_header();
        assert!(stream.starts_with(b"fLaC"));
        let mut input: Vec<i16> = Vec::new();
        for block in 0..60 {
            let samples: Vec<i16> = (0..block_frames * 2)
                .map(|i| {
                    let phase = (block * block_frames * 2 + i) as f64 / 40.0;
                    (phase.sin() * 12_000.0) as i16 ^ (i as i16 & 3)
                })
                .collect();
            stream.extend(encoder.encode(pcm(&samples)).expect("encodes"));
            input.extend(samples);
        }
        let (info, decoded) = decode(&stream);
        assert_eq!(info.sample_rate, rate);
        assert_eq!(info.channels, 2);
        assert_eq!(info.bits_per_sample, 16);
        assert_eq!(info.samples, None, "STREAMINFO total samples is 0");
        assert_eq!(info.min_block_size as usize, block_frames);
        assert_eq!(info.max_block_size as usize, block_frames);
        assert_eq!(decoded.len(), input.len(), "every frame decodes");
        assert!(
            decoded.iter().zip(&input).all(|(d, i)| *d == i32::from(*i)),
            "lossless"
        );
        assert!(encoder.encode(vec![0; 8]).is_err(), "only whole blocks");
    }
    assert_eq!(StreamFormat::flac(32_000), None);
    assert_eq!(
        StreamFormat::wav(48_000)
            .expect("rate")
            .with_codec(Codec::Flac),
        StreamFormat::flac(48_000).expect("rate")
    );
    assert_eq!(Codec::from_name(Codec::Flac.name()), Some(Codec::Flac));
    assert_eq!(Codec::from_name(Codec::Wav.name()), Some(Codec::Wav));
}

// covers: C72
#[test]
fn the_sender_adds_no_encoder_delay_and_at_most_its_2_s_queue() {
    for format in [
        StreamFormat::wav(44_100).expect("rate"),
        StreamFormat::flac(48_000).expect("rate"),
    ] {
        assert_eq!(format.encoder_delay(), Duration::ZERO, "{format:?}");
        // One pump block in, one FLAC frame out: nothing waits in the
        // encoder.
        let mut encoder = format.encoder().expect("encoder");
        let block = vec![1u8; format.block_frames() * 4];
        assert!(!encoder.encode(block).expect("encodes").is_empty());
    }
    assert_eq!(QUEUE_LIMIT, Duration::from_secs(2));
}

#[test]
fn the_stream_server_serves_flac_and_switches_codec_under_the_same_token() {
    let (writer, reader) = audio_ring(48_000);
    let config = StreamConfig {
        local_ip: LOCAL,
        allowed: vec![LOCAL],
        format: StreamFormat::wav(48_000).expect("rate"),
    };
    let server = StreamServer::start(config, reader).expect("starts");
    let _feed = feed(writer, 48_000);
    let wav_url = server.url();
    let flac_url = server.set_codec(Codec::Flac);
    assert!(wav_url.ends_with(".wav") && flac_url.ends_with(".flac"));
    assert_eq!(server.url(), flac_url);
    assert!(flac_url.contains(server.token()));
    assert_eq!(server.format().codec(), Codec::Flac);

    let (address, old_path) = http::split_url(&wav_url).expect("url");
    let mut old =
        http::request(address, &format!("GET {old_path} HTTP/1.1\r\n\r\n")).expect("connects");
    let (head, _) = http::read_response(&mut old, Duration::ZERO);
    assert_eq!(http::status(&head), 404, "the WAV path is gone");

    let (_, path) = http::split_url(&flac_url).expect("url");
    let mut stream =
        http::request(address, &format!("GET {path} HTTP/1.1\r\n\r\n")).expect("connects");
    let (head, body) = http::read_response(&mut stream, Duration::from_millis(400));
    assert_eq!(http::status(&head), 200);
    assert_eq!(http::header(&head, "Content-Type"), Some("audio/flac"));
    let bytes: Vec<u8> = http::chunks(&body).concat();
    let (info, samples) = decode(&bytes);
    assert_eq!(info.sample_rate, 48_000);
    assert!(!samples.is_empty(), "audio frames follow the header");
    assert!(
        samples.iter().any(|sample| *sample != 0),
        "the fed audio, not only silence"
    );
    server.stop();
}

fn config(device: &FakeDevice, format: StreamFormat) -> SessionConfig {
    SessionConfig {
        addresses: vec![device.address],
        format,
        metadata: Metadata::default(),
        artwork: None,
        group: None,
        fingerprints: None,
    }
}

fn try_start(
    device: &FakeDevice,
    format: StreamFormat,
) -> (Result<CastSession, SessionError>, Feed) {
    let (writer, reader) = audio_ring(48_000);
    let feed = feed(writer, 48_000);
    let selected = Arc::new(AtomicBool::new(true));
    let session = CastSession::start(config(device, format), reader, selected, |_, _| {});
    (session, feed)
}

fn content_types(device: &FakeDevice) -> Vec<String> {
    device
        .payloads(NS_MEDIA, "LOAD")
        .iter()
        .map(|load| {
            load["media"]["contentType"]
                .as_str()
                .unwrap_or("")
                .to_owned()
        })
        .collect()
}

// covers: C84
#[test]
fn a_device_that_refuses_wav_before_playing_gets_flac_once() {
    let wav = StreamFormat::wav(48_000).expect("rate");
    let refusals = [
        OnLoad::Error("LOAD_FAILED", json!({})),
        OnLoad::IdleBeforePlaying("ERROR"),
    ];
    for refusal in refusals {
        let device = FakeDevice::start(Behavior {
            on_load_of: Some(("audio/wav", refusal.clone())),
            ..Behavior::default()
        });
        let (session, _feed) = try_start(&device, wav);
        let mut session = session.unwrap_or_else(|error| panic!("{refusal:?}: {error:?}"));
        assert_eq!(session.format().codec(), Codec::Flac);
        assert_eq!(content_types(&device), ["audio/wav", "audio/flac"]);
        let loads = device.payloads(NS_MEDIA, "LOAD");
        let url = loads[1]["media"]["contentId"].as_str().expect("url");
        assert!(url.ends_with(".flac"), "{url}");
        assert_eq!(session.stream_url(), url);
        let events: Vec<SessionEvent> = std::iter::from_fn(|| session.poll_event()).collect();
        assert!(events.iter().any(|event| matches!(
            event,
            SessionEvent::FormatFallback {
                from: Codec::Wav,
                to: Codec::Flac,
                ..
            }
        )));
        let fetch = device.fetches().last().cloned().expect("a fetch");
        assert_eq!(
            http::header(&fetch.head, "Content-Type"),
            Some("audio/flac")
        );
        assert!(http::chunks(&fetch.body)[0].starts_with(b"fLaC"));
        session.stop();
        device.join();
    }

    // A second failure ends the attempt: two LOADs, never a third.
    let device = FakeDevice::start(Behavior {
        on_load: OnLoad::Error("LOAD_FAILED", json!({"detailedErrorCode": 104})),
        ..Behavior::default()
    });
    let (result, _feed) = try_start(&device, wav);
    assert!(matches!(
        result,
        Err(SessionError::Media(MediaError::LoadFailed { .. }))
    ));
    assert_eq!(content_types(&device), ["audio/wav", "audio/flac"]);
    device.join();

    // Only WAV falls back, and only for a refusal of the format.
    assert!(!falls_back_to_flac(
        Codec::Flac,
        &MediaError::LoadFailed {
            detailed_error_code: None
        }
    ));
    for other in [
        MediaError::NoFetch,
        MediaError::Timeout,
        MediaError::Idle(IdleReason::Cancelled),
    ] {
        assert!(!falls_back_to_flac(Codec::Wav, &other), "{other:?}");
    }
}
