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
/// Reads this far either side of the first and last onset feed the timeline.
pub const FIT_MARGIN_FRAMES: u64 = 44_100;

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
/// `anchor + f * (1 / RATE + slope)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Timeline {
    pub anchor: f64,
    pub slope: f64,
    pub reads: usize,
}

impl Timeline {
    pub fn seconds(&self, frame: u64) -> f64 {
        self.anchor + frame as f64 * (1.0 / RATE + self.slope)
    }

    pub fn frames_per_s(&self) -> f64 {
        1.0 / (1.0 / RATE + self.slope)
    }
}

/// Fits the timeline to reads given as (frames read so far, arrival minus
/// those frames at RATE), in frame order.
// A read arrives when its last frame plays or later, never earlier, so the
// line under every read that is highest at their mean frame fits best.
pub fn fit_timeline(reads: &[(u64, f64)]) -> Option<Timeline> {
    let mut hull: Vec<(f64, f64)> = Vec::new();
    for &(end, offset) in reads {
        let point = (end as f64, offset);
        while let [.., o, a] = hull[..] {
            let turn = (a.0 - o.0) * (point.1 - o.1) - (a.1 - o.1) * (point.0 - o.0);
            if turn > 0.0 {
                break;
            }
            hull.pop();
        }
        hull.push(point);
    }
    let first = *hull.first()?;
    let mean = reads.iter().map(|&(end, _)| end as f64).sum::<f64>() / reads.len() as f64;
    let (left, right) = hull
        .windows(2)
        .find(|edge| edge[1].0 >= mean)
        .map_or((first, first), |edge| (edge[0], edge[1]));
    let slope = if right.0 > left.0 {
        (right.1 - left.1) / (right.0 - left.0)
    } else {
        0.0
    };
    Some(Timeline {
        anchor: left.1 - slope * left.0,
        slope,
        reads: reads.len(),
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

    /// At the end of input: fits the timeline to the reads around the onsets,
    /// which leaves out the receiver's start-up flush and its stop, and times
    /// every onset on it.
    pub fn finish(&mut self) {
        let span = match (self.onset_frames.first(), self.onset_frames.last()) {
            (Some(&first), Some(&last)) => {
                first.saturating_sub(FIT_MARGIN_FRAMES)..=last + FIT_MARGIN_FRAMES
            }
            _ => 0..=u64::MAX,
        };
        let near: Vec<(u64, f64)> = self
            .reads
            .iter()
            .copied()
            .filter(|(end, _)| span.contains(end))
            .collect();
        self.timeline = fit_timeline(if near.len() >= 2 { &near } else { &self.reads });
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
