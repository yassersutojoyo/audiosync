//! The two-speaker sync check of the spec row `hardware_two_speaker_sync`:
//! the receiver list with per-speaker delays and the click `rx_pair` sends,
//! the onset detector `rx_onset` runs on each receiver's PCM, and the
//! verdict that compares the onsets with the delays (P102).

// Each example includes this file and uses only its own half.
#![allow(dead_code)]

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
/// Reads in this stretch before the first onset, inside the click source's
/// leading silence and after a receiver's start-up flush, set the anchor.
pub const ANCHOR_WINDOW_FRAMES: u64 = 3 * 44_100;

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
    /// When the frame plays on the receiver's timeline, on the reader's clock.
    pub seconds: f64,
}

/// A receiver's play timeline on the reader's clock: frame f plays at
/// `anchor + f / RATE`, the rate every receiver plays at (P95).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timeline {
    pub anchor: f64,
    pub reads: usize,
}

impl Timeline {
    pub fn seconds(&self, frame: u64) -> f64 {
        self.anchor + frame as f64 / RATE
    }
}

/// The timeline from reads given as (frames read so far, arrival minus those
/// frames at RATE), taken in the window before frame `first_onset`.
// A pipe receiver's writes slip by whole packets later in a run and catch up
// in bursts (WSL read logs), so only the stretch before the first click marks
// when it lined its output up; a low percentile there skips packet-late reads.
pub fn fit_timeline(reads: &[(u64, f64)], first_onset: u64) -> Option<Timeline> {
    let window = first_onset.saturating_sub(ANCHOR_WINDOW_FRAMES)..first_onset;
    let mut offsets: Vec<f64> = reads
        .iter()
        .filter(|(end, _)| window.contains(end))
        .map(|&(_, offset)| offset)
        .collect();
    if offsets.is_empty() {
        offsets = reads.iter().map(|&(_, offset)| offset).collect();
    }
    offsets.sort_by(f64::total_cmp);
    let anchor = *offsets.get((offsets.len().max(1) - 1) * 5 / 100)?;
    Some(Timeline {
        anchor,
        reads: offsets.len(),
    })
}

/// Finds click onsets in one receiver's 16-bit little-endian stereo PCM and
/// times them on the receiver's play timeline, fitted to when reads arrived.
pub struct Detector {
    threshold: i32,
    frames: u64,
    carry: Vec<u8>,
    last_loud: Option<u64>,
    /// (frames read so far, arrival minus those frames at RATE) per read.
    reads: Vec<(u64, f64)>,
    onset_frames: Vec<u64>,
    timeline: Option<Timeline>,
    onsets: Vec<Onset>,
}

impl Detector {
    pub fn new(threshold: i32) -> Detector {
        Detector {
            threshold,
            frames: 0,
            carry: Vec::new(),
            last_loud: None,
            reads: Vec::new(),
            onset_frames: Vec::new(),
            timeline: None,
            onsets: Vec::new(),
        }
    }

    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Empty until `finish` times them.
    pub fn onsets(&self) -> &[Onset] {
        &self.onsets
    }

    pub fn timeline(&self) -> Option<Timeline> {
        self.timeline
    }

    /// (frames read so far, arrival minus those frames at RATE) per read.
    pub fn reads(&self) -> &[(u64, f64)] {
        &self.reads
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
                    self.onset_frames.push(self.frames);
                }
                self.last_loud = Some(self.frames);
            }
            self.frames += 1;
        }
        self.carry.drain(..whole);
        if whole > 0 {
            self.reads
                .push((self.frames, arrival - self.frames as f64 / RATE));
        }
    }

    /// At the end of input: anchors the timeline before the first onset and
    /// times every onset on it.
    pub fn finish(&mut self) {
        let first = self.onset_frames.first().copied().unwrap_or(u64::MAX);
        self.timeline = fit_timeline(&self.reads, first);
        let timeline = self.timeline;
        self.onsets = self
            .onset_frames
            .iter()
            .map(|&frame| Onset {
                frame,
                seconds: timeline.map_or(f64::NAN, |line| line.seconds(frame)),
            })
            .collect();
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
    /// The other stream's onset frame minus the reference's.
    pub frame_diff: i64,
    pub pass: bool,
}

#[derive(Debug, PartialEq)]
pub struct Verdict {
    pub pairs: Vec<Pair>,
    /// Largest minus smallest `frame_diff`: how far the streams drifted.
    pub drift_frames: u64,
    pub drift_pass: bool,
    pub pass: bool,
    pub reason: String,
}

/// Timeline frame `frame`'s RTP timestamp on one speaker and the play time
/// the sender stated for it: k_s from the speaker's first (packet, RTP
/// timestamp) (P81), then the latest sync (RTP timestamp, play time) at or
/// before it, at 44.1 kHz from there as a receiver places frames (P95, P97).
pub fn stated_play(first: (u64, u32), syncs: &[(u32, f64)], frame: i64) -> Option<(u32, f64)> {
    let (packet, rtptime) = first;
    let k = rtptime.wrapping_sub((packet * PACKET_FRAMES) as u32);
    let rtp = k.wrapping_add(frame as u32);
    let (since, at) = syncs
        .iter()
        .map(|&(heard, at)| (rtp.wrapping_sub(heard) as i32, at))
        .filter(|&(since, _)| since >= 0)
        .min_by_key(|&(since, _)| since)?;
    Some((rtp, at + f64::from(since) / RATE))
}

/// P102 on the two streams: the k-th onset of each is the k-th click; their
/// time difference must equal the delay difference within one packet, and
/// their frame offset must stay the same within one packet. The last click
/// of the later speaker may fall after the end of the run, so only the
/// clicks both streams heard are compared.
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
                frame_diff: b.frame as i64 - a.frame as i64,
                pass: error_frames.abs() <= PACKET_FRAMES as f64,
            }
        })
        .collect();
    let diffs = pairs.iter().map(|pair| pair.frame_diff);
    let drift_frames = match (diffs.clone().min(), diffs.max()) {
        (Some(low), Some(high)) => high.abs_diff(low),
        _ => 0,
    };
    let drift_pass = drift_frames <= PACKET_FRAMES;
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
    } else if !drift_pass {
        (
            false,
            format!("the streams drifted {drift_frames} frames apart"),
        )
    } else {
        (true, format!("{} clicks within one packet", pairs.len()))
    };
    Verdict {
        pairs,
        drift_frames,
        drift_pass,
        pass,
        reason,
    }
}
