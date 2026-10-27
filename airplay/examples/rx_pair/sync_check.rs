//! The two-speaker sync check of the spec row `hardware_two_speaker_sync`:
//! the receiver list with per-speaker delays and the click `rx_pair` sends,
//! the onset detector `rx_onset` runs on each receiver's PCM, and the
//! verdict that compares the onsets with the delays (P102).

// Each example includes this file and uses only its own half.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::time::Duration;

use audiosync_airplay::audio::PACKET_FRAMES;
use audiosync_airplay::discovery::ProtocolPath;

pub const RATE: f64 = 44_100.0;
/// The largest d_s the sender takes (P99).
pub const MAX_DELAY_MS: u64 = 20_000;

/// Late enough that every receiver on loopback is playing before the first.
pub const CLICK_FIRST_FRAME: u64 = 6 * 44_100;
/// Far longer than any delay difference the check uses, so the k-th onset
/// of one stream can't pair with another click of the other.
pub const CLICK_PERIOD_FRAMES: u64 = 4 * 44_100;
pub const CLICK_FRAMES: u64 = 64;
/// -6 dBFS: still about -26 dBFS after the -20 dB speaker volume.
pub const CLICK_LEVEL: f32 = 0.5;

/// 16-bit level an onset must reach: about -38 dBFS, far above dither.
pub const DEFAULT_THRESHOLD: i32 = 400;
/// Quiet frames after a click before the detector takes a new onset.
pub const REARM_FRAMES: u64 = 44_100;
/// Chunks whose last frame lies this close to an onset time it.
pub const WINDOW_FRAMES: u64 = 2_205;

/// One `--rx` value: `host:port[,ap1|ap2][,delay=<ms>]`.
#[derive(Debug, PartialEq)]
pub struct RxSpec {
    pub host_port: String,
    pub path: ProtocolPath,
    pub delay: Duration,
}

pub fn parse_rx_spec(text: &str) -> Result<RxSpec, String> {
    let mut parts = text.split(',');
    let host_port = parts.next().unwrap_or_default().to_owned();
    if host_port.is_empty() {
        return Err(format!("--rx {text}: no host:port"));
    }
    let mut spec = RxSpec {
        host_port,
        path: ProtocolPath::AirPlay2,
        delay: Duration::ZERO,
    };
    for part in parts {
        match part {
            "ap1" => spec.path = ProtocolPath::AirPlay1,
            "ap2" => spec.path = ProtocolPath::AirPlay2,
            _ => {
                let Some(value) = part.strip_prefix("delay=") else {
                    return Err(format!("unknown option '{part}' in --rx {text}"));
                };
                let ms: u64 = value
                    .parse()
                    .map_err(|_| format!("--rx {text}: delay '{value}' is not whole ms"))?;
                if ms > MAX_DELAY_MS {
                    return Err(format!("--rx {text}: delay above {MAX_DELAY_MS} ms"));
                }
                spec.delay = Duration::from_millis(ms);
            }
        }
    }
    Ok(spec)
}

/// The click source's sample at capture frame `n`: silence with a short
/// burst every `CLICK_PERIOD_FRAMES` from `CLICK_FIRST_FRAME` on.
pub fn click_sample(n: u64) -> f32 {
    match n.checked_sub(CLICK_FIRST_FRAME) {
        Some(since) if since % CLICK_PERIOD_FRAMES < CLICK_FRAMES => CLICK_LEVEL,
        _ => 0.0,
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Onset {
    /// Frame index in the stream, from its first byte.
    pub frame: u64,
    /// When the frame came out of the receiver, on the reader's clock.
    pub seconds: f64,
}

/// Finds click onsets in one receiver's 16-bit little-endian stereo PCM and
/// times them by when the bytes arrived.
pub struct Detector {
    threshold: i32,
    frames: u64,
    carry: Vec<u8>,
    last_loud: Option<u64>,
    /// (frames read so far, arrival seconds) per read.
    chunks: VecDeque<(u64, f64)>,
    pending: VecDeque<u64>,
    onsets: Vec<Onset>,
}

impl Detector {
    pub fn new(threshold: i32) -> Detector {
        Detector {
            threshold,
            frames: 0,
            carry: Vec::new(),
            last_loud: None,
            chunks: VecDeque::new(),
            pending: VecDeque::new(),
            onsets: Vec::new(),
        }
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn onsets(&self) -> &[Onset] {
        &self.onsets
    }

    /// One read's bytes, which may split a frame, and when the read returned.
    pub fn push(&mut self, bytes: &[u8], arrival: f64) {
        self.carry.extend_from_slice(bytes);
        let whole = self.carry.len() / 4 * 4;
        for frame in self.carry[..whole].as_chunks::<4>().0 {
            let left = i16::from_le_bytes([frame[0], frame[1]]) as i32;
            let right = i16::from_le_bytes([frame[2], frame[3]]) as i32;
            if left.abs().max(right.abs()) >= self.threshold {
                let rearmed = self
                    .last_loud
                    .is_none_or(|last| self.frames - last > REARM_FRAMES);
                if rearmed {
                    self.pending.push_back(self.frames);
                }
                self.last_loud = Some(self.frames);
            }
            self.frames += 1;
        }
        self.carry.drain(..whole);
        if whole > 0 {
            self.chunks.push_back((self.frames, arrival));
        }
        while let Some(&onset) = self.pending.front() {
            if onset + WINDOW_FRAMES > self.frames {
                break;
            }
            self.pending.pop_front();
            self.settle(onset);
        }
        let keep_from = self.frames.saturating_sub(2 * WINDOW_FRAMES);
        let oldest = self.pending.front().map_or(keep_from, |&p| {
            keep_from.min(p.saturating_sub(WINDOW_FRAMES))
        });
        while self.chunks.len() > 1 && self.chunks[0].0 < oldest {
            self.chunks.pop_front();
        }
    }

    /// Times the onsets still waiting for their window, at the end of input.
    pub fn finish(&mut self) {
        while let Some(onset) = self.pending.pop_front() {
            self.settle(onset);
        }
    }

    // A paced writer's bytes arrive at their write time or later, never
    // earlier, so the least delayed nearby read gives the best time.
    fn settle(&mut self, onset: u64) {
        let estimate = |&(end, arrival): &(u64, f64)| arrival - (end as f64 - onset as f64) / RATE;
        let near = self
            .chunks
            .iter()
            .filter(|(end, _)| end.abs_diff(onset) <= WINDOW_FRAMES)
            .map(estimate)
            .fold(f64::INFINITY, f64::min);
        let seconds = if near.is_finite() {
            near
        } else {
            // One read larger than the window: the read that carried it.
            self.chunks
                .iter()
                .find(|(end, _)| *end > onset)
                .map_or(f64::NAN, estimate)
        };
        self.onsets.push(Onset {
            frame: onset,
            seconds,
        });
    }
}

#[derive(Debug, PartialEq)]
pub struct Pair {
    pub index: usize,
    pub reference: f64,
    pub other: f64,
    pub diff_s: f64,
    pub expected_s: f64,
    pub error_frames: f64,
    pub pass: bool,
}

#[derive(Debug, PartialEq)]
pub struct Verdict {
    pub pairs: Vec<Pair>,
    pub pass: bool,
    pub reason: String,
}

/// P102 on the two streams: the k-th onset of each is the k-th click, and
/// their time difference must equal the delay difference within one packet.
/// The last click of the later speaker may fall after the end of the run, so
/// only the clicks both streams heard are compared.
pub fn verdict(reference: (&[Onset], Duration), other: (&[Onset], Duration)) -> Verdict {
    let expected_s = other.1.as_secs_f64() - reference.1.as_secs_f64();
    let pairs: Vec<Pair> = reference
        .0
        .iter()
        .zip(other.0)
        .enumerate()
        .map(|(index, (a, b))| {
            let diff_s = b.seconds - a.seconds;
            let error_frames = (diff_s - expected_s) * RATE;
            Pair {
                index: index + 1,
                reference: a.seconds,
                other: b.seconds,
                diff_s,
                expected_s,
                error_frames,
                pass: error_frames.abs() <= PACKET_FRAMES as f64,
            }
        })
        .collect();
    let failed = pairs.iter().filter(|pair| !pair.pass).count();
    let (pass, reason) = if pairs.is_empty() {
        (false, "a receiver gave no onset".to_owned())
    } else if failed > 0 {
        (
            false,
            format!(
                "{failed} of {} clicks off by more than one packet",
                pairs.len()
            ),
        )
    } else {
        (true, format!("{} clicks within one packet", pairs.len()))
    };
    Verdict {
        pairs,
        pass,
        reason,
    }
}
