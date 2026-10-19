//! The audio format, the conversions and the payloads (section 5 of
//! docs/AIRPLAY_SPEC.md): float and 16-bit input, PCM payloads and ALAC
//! frames checked against a reference decoder.

mod common;

use std::time::Duration;

use audiosync_airplay::alac;
use audiosync_airplay::audio::{
    self, CHANNELS, FRAMES_PER_PACKET, Frame, PCM_PAYLOAD_BYTES, SAMPLE_RATE, sample_from_f32,
    write_pcm_payload,
};
use audiosync_airplay::sdp::{ALAC_FMTP, Codec};
use common::rig::{Host, Rig, decode, host_frame, speaker};

// covers: P72, P73, P76
#[test]
fn pcm_payload_big_endian_from_float_or_16_bit_input() {
    // The stream format (P72).
    assert_eq!((SAMPLE_RATE, CHANNELS), (44_100, 2));

    // Float to 16-bit: × 32 767, rounded to nearest, clamped; NaN and
    // infinities become 0 (P73).
    assert_eq!(sample_from_f32(0.0), 0);
    assert_eq!(sample_from_f32(-0.0), 0);
    assert_eq!(sample_from_f32(1.0), 32_767);
    assert_eq!(sample_from_f32(-1.0), -32_767);
    assert_eq!(
        sample_from_f32(0.5),
        16_384,
        "16 383.5 rounds away from zero"
    );
    assert_eq!(sample_from_f32(-0.5), -16_384);
    assert_eq!(sample_from_f32(0.25), 8_192, "8 191.75");
    assert_eq!(sample_from_f32(1.5), 32_767, "clamped");
    assert_eq!(
        sample_from_f32(-1.5),
        -32_768,
        "clamped to the 16-bit range"
    );
    assert_eq!(sample_from_f32(f32::NAN), 0);
    assert_eq!(sample_from_f32(f32::INFINITY), 0);
    assert_eq!(sample_from_f32(f32::NEG_INFINITY), 0);

    // 352 frames × 2 channels × 2 bytes, each sample big-endian, left
    // before right (P76).
    assert_eq!(PCM_PAYLOAD_BYTES, 1408);
    let mut frames = [[0i16; 2]; FRAMES_PER_PACKET];
    frames[0] = [0x1234, -2];
    frames[351] = [i16::MIN, i16::MAX];
    let mut payload = [0u8; PCM_PAYLOAD_BYTES];
    write_pcm_payload(&frames, &mut payload);
    assert_eq!(&payload[..4], &[0x12, 0x34, 0xFF, 0xFE]);
    assert_eq!(&payload[1404..], &[0x80, 0x00, 0x7F, 0xFF]);
    // With the 12-byte RTP header it fits a 1500-byte MTU over IPv6 (40)
    // and UDP (8).
    const { assert!(12 + PCM_PAYLOAD_BYTES + 8 + 40 <= 1500) };

    // Through the stream: float input reaches a PCM speaker converted, as
    // big-endian samples in 1408-byte payloads.
    let mut rig = Rig::with_host();
    let mut host = Host::new(0.0);
    host.float = true;
    rig.host = Some(host);
    let pcm = rig.add(speaker(1, Codec::Pcm));
    rig.run_for(Duration::from_millis(500));
    let audio = rig.audio(pcm);
    assert!(audio.len() > 40);
    for packet in &audio[1..] {
        assert_eq!(packet.payload.len(), PCM_PAYLOAD_BYTES);
        let n = rig.frame_of(pcm, packet.header.timestamp);
        let frames = decode(Codec::Pcm, &packet.payload);
        for (offset, frame) in frames.iter().enumerate() {
            let expected = host_frame(n as u64 + offset as u64);
            // The host wrote s / 32 767 as float; it comes back as s.
            assert_eq!(*frame, expected, "frame {}", n + offset as i64);
        }
    }
}

fn round_trip(frames: &[Frame; FRAMES_PER_PACKET]) -> Vec<Frame> {
    let mut out = [0u8; alac::FRAME_BYTES];
    alac::write_frame(frames, &mut out);
    decode(Codec::Alac, &out)
}

// covers: P77, P29
#[test]
fn alac_frames_decode_back_with_a_decoder_set_up_from_the_fmtp_line() {
    // The configuration record carries the fmtp values in Apple's order
    // (P29): 352 0 16 40 10 14 2 255 0 0 44100.
    let config = alac::configuration();
    let fields: Vec<u64> = vec![
        u64::from(u32::from_be_bytes([
            config[0], config[1], config[2], config[3],
        ])),
        u64::from(config[4]),
        u64::from(config[5]),
        u64::from(config[6]),
        u64::from(config[7]),
        u64::from(config[8]),
        u64::from(config[9]),
        u64::from(u16::from_be_bytes([config[10], config[11]])),
        u64::from(u32::from_be_bytes([
            config[12], config[13], config[14], config[15],
        ])),
        u64::from(u32::from_be_bytes([
            config[16], config[17], config[18], config[19],
        ])),
        u64::from(u32::from_be_bytes([
            config[20], config[21], config[22], config[23],
        ])),
    ];
    let fmtp: Vec<u64> = ALAC_FMTP
        .split(' ')
        .map(|field| field.parse().expect("number"))
        .collect();
    assert_eq!(fields, fmtp);

    // Every frame holds exactly 352 frames and decodes back to the input,
    // whatever the signal: silence, full scale, a ramp, noise.
    let mut signals = vec![
        [[0i16; 2]; FRAMES_PER_PACKET],
        [[i16::MAX, i16::MIN]; FRAMES_PER_PACKET],
        [[i16::MIN, i16::MAX]; FRAMES_PER_PACKET],
    ];
    let mut ramp = [[0i16; 2]; FRAMES_PER_PACKET];
    for (index, frame) in ramp.iter_mut().enumerate() {
        *frame = host_frame(index as u64 * 37);
    }
    signals.push(ramp);
    let mut noise = [[0i16; 2]; FRAMES_PER_PACKET];
    let mut rng = common::rig::Rng::new(77);
    for frame in noise.iter_mut() {
        *frame = [rng.next_u64() as i16, rng.next_u64() as i16];
    }
    signals.push(noise);
    for frames in &signals {
        let back = round_trip(frames);
        assert_eq!(back.len(), FRAMES_PER_PACKET);
        assert_eq!(&back[..], &frames[..]);
    }

    // An uncompressed frame is 1412 bytes and fits the MTU like PCM.
    assert_eq!(alac::FRAME_BYTES, 1412);
    const { assert!(12 + alac::FRAME_BYTES + 8 + 40 <= 1500) };

    // Through the stream: an ALAC speaker's payloads decode back to the
    // host's frames at their timeline positions.
    let mut rig = Rig::with_host();
    let id = rig.add(speaker(1, Codec::Alac));
    rig.run_for(Duration::from_millis(500));
    let audio = rig.audio(id);
    assert!(audio.len() > 40);
    for packet in &audio[1..] {
        assert_eq!(packet.payload.len(), alac::FRAME_BYTES);
        let n = rig.frame_of(id, packet.header.timestamp) as u64;
        let frames = decode(Codec::Alac, &packet.payload);
        let expected: Vec<Frame> = (n..n + FRAMES_PER_PACKET as u64).map(host_frame).collect();
        assert_eq!(frames, expected);
    }
}

// Part of P121 (the per-speaker encoding is tested with the stream).
#[test]
fn software_gain_is_ten_to_the_db_over_twenty() {
    use audiosync_airplay::volume::Volume;
    assert_eq!(audio::software_gain(Volume::Db(0.0)), 1.0);
    assert!((audio::software_gain(Volume::Db(-20.0)) - 0.1).abs() < 1e-12);
    assert!((audio::software_gain(Volume::Db(-6.0)) - 0.501_187).abs() < 1e-6);
    assert_eq!(audio::software_gain(Volume::Muted), 0.0);
    let mut frames = [[10_000i16, -10_000], [i16::MAX, i16::MIN], [3, -3]];
    audio::apply_gain(&mut frames, 0.5);
    assert_eq!(frames, [[5_000, -5_000], [16_384, -16_384], [2, -2]]);
    let mut loud = [[30_000i16, -30_000]];
    audio::apply_gain(&mut loud, 2.0);
    assert_eq!(loud, [[i16::MAX, i16::MIN]], "clamped");
}
