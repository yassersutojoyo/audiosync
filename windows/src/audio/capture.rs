//! What process loopback captures, which sources to offer, the formats a
//! capture asks Windows for, and the conversion of captured packets into the
//! shared ring, through the equalizer, and into the AirPlay sender. An input
//! device is captured from its own capture endpoint instead, through the same
//! formats and conversion.
//!
//! Process loopback captures the render streams of a process tree: either
//! only that tree (one app), or everything except it (the whole system,
//! without AudioSync). AudioSync must never capture its own output, or the
//! delayed copies would be captured again and echo (spec S10). So the whole
//! system excludes AudioSync's own tree, and an app is refused when its tree
//! contains AudioSync, or AudioSync's tree contains it. Both are decided on
//! process creation times where they can be read (see [`super::process`]),
//! so an app that only got the ID of an exited process in AudioSync's
//! ancestry can be captured. Where they can't be read, or AudioSync is
//! missing from the snapshot, AudioSync refuses rather than capture itself.

use std::time::Instant;

use audiosync_core::channels::input_to_stereo;
use audiosync_core::clock;
use audiosync_core::eq::EqProcessor;
use audiosync_core::resample::Resampler;
use audiosync_core::ring::{RingWriter, is_supported_rate};

use super::SESSION_RATE;
use super::airplay::FeedLease;
use super::backend::{BackendError, SourceId, SourceInfo};
use super::outputs::{EndpointReading, is_hands_free};
use super::process::{IDLE_PID, ProcessTable, SYSTEM_PID};

/// The name of the whole-system source in English, as the macOS app shows
/// it: for logs and the mock. The UI, and AirPlay receivers while no track
/// counts, show its own, translated label.
pub const WHOLE_SYSTEM_NAME: &str = "All System Audio";

/// Which render streams a process loopback capture includes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoopbackMode {
    /// Only the target process and its descendants
    /// (`PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE`).
    IncludeTree,
    /// Every process except the target and its descendants
    /// (`PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE`).
    ExcludeTree,
}

/// How to activate a process loopback capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Activation {
    /// Include or exclude the target's tree.
    pub mode: LoopbackMode,
    /// The process whose tree is included or excluded.
    pub target_pid: u32,
}

/// What a capture opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CaptureTarget {
    /// A process loopback capture.
    Loopback(Activation),
    /// The capture endpoint of an input device, by endpoint ID.
    Input {
        /// The endpoint ID.
        endpoint_id: String,
    },
}

/// The capture for `source`: an input device's own endpoint, or the process
/// loopback capture [`decide_activation`] decides on.
pub fn decide_capture(
    source: &SourceId,
    own_pid: u32,
    processes: &ProcessTable,
) -> Result<CaptureTarget, BackendError> {
    match source {
        SourceId::Input { endpoint_id } => Ok(CaptureTarget::Input {
            endpoint_id: endpoint_id.clone(),
        }),
        _ => decide_activation(source, own_pid, processes).map(CaptureTarget::Loopback),
    }
}

/// The process loopback capture for `source`, with AudioSync running as
/// `own_pid` and the running processes in `processes`. An input device has
/// none ([`BackendError::NotAnApp`]): [`decide_capture`] opens its endpoint.
///
/// The whole system excludes AudioSync's own tree. An app includes the tree
/// of its top process, unless that process is not an app, no longer runs,
/// is AudioSync or one of its descendants, or may be an ancestor of AudioSync
/// (which happens when the user picks the app that started AudioSync, such
/// as Explorer or a terminal). An app that got the ID of a process in
/// AudioSync's ancestry after that process exited is no ancestor when the
/// creation times show it started after the process that names that ID as
/// its parent. When they can't show it, it counts as one, and every app does
/// while AudioSync is missing from `processes`.
pub fn decide_activation(
    source: &SourceId,
    own_pid: u32,
    processes: &ProcessTable,
) -> Result<Activation, BackendError> {
    let root_pid = match source {
        SourceId::WholeSystem => {
            return Ok(Activation {
                mode: LoopbackMode::ExcludeTree,
                target_pid: own_pid,
            });
        }
        SourceId::App { root_pid } => *root_pid,
        SourceId::Input { .. } => return Err(BackendError::NotAnApp),
    };
    if root_pid == IDLE_PID || root_pid == SYSTEM_PID {
        return Err(BackendError::NotAnApp);
    }
    if root_pid == own_pid {
        return Err(BackendError::OwnProcess);
    }
    if !processes.contains(root_pid) {
        return Err(BackendError::SourceGone);
    }
    // A process created before AudioSync is never one AudioSync started,
    // whatever ID it names as its parent. Capturing its tree can't capture
    // AudioSync unless AudioSync is in that tree, which the next check
    // decides.
    if processes.in_tree(root_pid, own_pid) {
        return Err(BackendError::OwnProcess);
    }
    // AudioSync's ancestors: the walk up its recorded parent IDs stops where
    // a process created after the child holds the ID, and goes on where a
    // creation time is unknown. Without AudioSync's own entry, nothing is
    // ruled out.
    if !processes.contains(own_pid) || processes.in_tree(own_pid, root_pid) {
        return Err(BackendError::ContainsAudioSync);
    }
    Ok(Activation {
        mode: LoopbackMode::IncludeTree,
        target_pid: root_pid,
    })
}

/// The name to show for an executable: its file name without `.exe`, with a
/// capital first letter (`spotify.exe` becomes `Spotify`).
pub fn display_name(executable: &str) -> String {
    let stem = match executable.len().checked_sub(4) {
        Some(cut)
            if executable.is_char_boundary(cut)
                && executable[cut..].eq_ignore_ascii_case(".exe") =>
        {
            &executable[..cut]
        }
        _ => executable,
    };
    let mut chars = stem.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => executable.to_owned(),
    }
}

/// The sources to offer: the whole system, then one entry per app that owns
/// one of the audio sessions `session_pids`, by name.
///
/// Each session's process leads to its app's top process
/// ([`ProcessTable::app_root`]), so an app with several audio processes is
/// listed once. Processes that no longer run, and apps that
/// [`decide_activation`] would refuse (AudioSync's own tree and its
/// ancestors), are left out.
pub fn source_list(
    processes: &ProcessTable,
    session_pids: &[u32],
    own_pid: u32,
) -> Vec<SourceInfo> {
    let mut apps: Vec<SourceInfo> = Vec::new();
    for &pid in session_pids {
        if !processes.contains(pid) {
            continue;
        }
        let root_pid = processes.app_root(pid);
        let source = SourceId::App { root_pid };
        if apps.iter().any(|app| app.id == source)
            || decide_activation(&source, own_pid, processes).is_err()
        {
            continue;
        }
        let executable = processes
            .get(root_pid)
            .map(|entry| entry.executable.clone())
            .unwrap_or_default();
        apps.push(SourceInfo {
            id: source,
            name: display_name(&executable),
            executable,
        });
    }
    apps.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.executable.cmp(&b.executable))
    });
    let mut sources = Vec::with_capacity(apps.len() + 1);
    sources.push(SourceInfo {
        id: SourceId::WholeSystem,
        name: WHOLE_SYSTEM_NAME.to_owned(),
        executable: String::new(),
    });
    sources.extend(apps);
    sources
}

/// The input devices to offer, from the active capture endpoints Windows
/// listed, by name, then endpoint ID. An endpoint without an ID can't be
/// opened, and a Bluetooth hands-free one is left out as it is for outputs
/// ([`is_hands_free`]): opening it switches the headset to call mode, which
/// interrupts its stereo sound. One without a readable name shows its ID.
pub fn input_sources(readings: Vec<EndpointReading>) -> Vec<SourceInfo> {
    let mut inputs: Vec<SourceInfo> = readings
        .into_iter()
        .filter_map(|reading| {
            let endpoint_id = reading.id.ok()?;
            let properties = reading.properties.unwrap_or_default();
            if is_hands_free(properties.enumerator.as_deref().unwrap_or_default()) {
                return None;
            }
            let name = properties
                .name
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| endpoint_id.clone());
            Some(SourceInfo {
                id: SourceId::Input { endpoint_id },
                name,
                executable: String::new(),
            })
        })
        .collect();
    inputs.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id_text().cmp(b.id_text()))
    });
    inputs
}

impl SourceInfo {
    /// The endpoint ID of an input device, empty for other sources.
    fn id_text(&self) -> &str {
        match &self.id {
            SourceId::Input { endpoint_id } => endpoint_id,
            _ => "",
        }
    }
}

/// The app that runs `executable` now, found among all running processes,
/// so it is found whether or not it has an audio session: the app of process
/// `root_pid` while that process still runs `executable`, else the app of
/// the first other process that does. `None` when no process runs it, so a
/// process ID that another program got after the app quit never counts.
///
/// Whether the app may be captured is left to [`decide_activation`].
pub fn running_app(
    processes: &ProcessTable,
    root_pid: u32,
    executable: &str,
) -> Option<SourceInfo> {
    let pid = processes
        .running(executable)
        .find(|&pid| pid == root_pid)
        .or_else(|| processes.running(executable).next())?;
    let root_pid = processes.app_root(pid);
    let executable = processes.get(root_pid)?.executable.clone();
    Some(SourceInfo {
        id: SourceId::App { root_pid },
        name: display_name(&executable),
        executable,
    })
}

/// A sample type a capture can deliver.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SampleType {
    /// 32-bit IEEE float.
    F32,
    /// 16-bit signed integer PCM.
    I16,
}

/// An interleaved PCM stream format.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamFormat {
    /// The sample type.
    pub sample: SampleType,
    /// Frames per second.
    pub rate: u32,
    /// Channels per frame.
    pub channels: u16,
}

impl StreamFormat {
    /// Bits per sample.
    pub fn bits(&self) -> u16 {
        match self.sample {
            SampleType::F32 => 32,
            SampleType::I16 => 16,
        }
    }

    /// Bytes per frame (`nBlockAlign`).
    pub fn block_align(&self) -> usize {
        usize::from(self.channels) * usize::from(self.bits() / 8)
    }
}

/// One way to initialize a process loopback capture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureAttempt {
    /// The format asked for. Process loopback has no mix format of its own,
    /// so the capture always names one.
    pub format: StreamFormat,
    /// Whether to let Windows convert to that format
    /// (`AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`).
    pub autoconvert: bool,
}

/// The ways a capture is initialized, tried in order until Windows accepts
/// one: stereo float at the session rate first, which needs no conversion
/// here, then 16-bit at the session rate, then 16-bit at 44.1 kHz, which
/// [`CaptureConverter`] converts.
pub const CAPTURE_ATTEMPTS: [CaptureAttempt; 4] = [
    CaptureAttempt {
        format: StreamFormat {
            sample: SampleType::F32,
            rate: SESSION_RATE,
            channels: 2,
        },
        autoconvert: true,
    },
    CaptureAttempt {
        format: StreamFormat {
            sample: SampleType::F32,
            rate: SESSION_RATE,
            channels: 2,
        },
        autoconvert: false,
    },
    CaptureAttempt {
        format: StreamFormat {
            sample: SampleType::I16,
            rate: SESSION_RATE,
            channels: 2,
        },
        autoconvert: false,
    },
    CaptureAttempt {
        format: StreamFormat {
            sample: SampleType::I16,
            rate: 44_100,
            channels: 2,
        },
        autoconvert: false,
    },
];

/// Frames converted per step; bounds the converter's buffers.
const CONVERT_CHUNK: usize = 1024;

/// Turns captured packets into interleaved stereo float at
/// [`SESSION_RATE`], equalizes them and writes them to the ring, and to the
/// AirPlay sender when it has its feed.
///
/// The equalizer runs at the session rate, after any resampling, so it is
/// designed for [`SESSION_RATE`] whatever rate Windows captures at, and every
/// output and AirPlay speaker hears the same equalized audio. All buffers are
/// allocated by [`CaptureConverter::new`]; [`CaptureConverter::push`] never
/// allocates, locks or blocks, and neither do the equalizer and the AirPlay
/// feed.
pub struct CaptureConverter {
    format: StreamFormat,
    writer: RingWriter,
    equalizer: EqProcessor,
    /// The AirPlay sender's feed, lent for this capture.
    airplay: Option<FeedLease>,
    /// From the capture rate to the session rate, when they differ.
    resampler: Option<Resampler>,
    /// One chunk of decoded samples, `channels` per frame.
    decoded: Box<[f32]>,
    /// One chunk in stereo.
    stereo: Box<[f32]>,
    /// Resampler output, stereo.
    resampled: Box<[f32]>,
}

impl CaptureConverter {
    /// A converter for packets in `format`, writing through `writer` after
    /// `equalizer`, whose control half publishes settings designed for
    /// [`SESSION_RATE`], and handing the same audio to `airplay`.
    pub fn new(
        format: StreamFormat,
        writer: RingWriter,
        equalizer: EqProcessor,
        airplay: Option<FeedLease>,
    ) -> Result<CaptureConverter, BackendError> {
        if format.channels == 0 || !is_supported_rate(format.rate) {
            return Err(BackendError::UnsupportedFormat);
        }
        let resampler = if format.rate == SESSION_RATE {
            None
        } else {
            Some(
                Resampler::new(format.rate, SESSION_RATE)
                    .map_err(|_| BackendError::UnsupportedFormat)?,
            )
        };
        let resampled_frames = (CONVERT_CHUNK as u64 * u64::from(SESSION_RATE))
            .div_ceil(u64::from(format.rate)) as usize
            + 2;
        Ok(CaptureConverter {
            format,
            writer,
            equalizer,
            airplay,
            resampler,
            decoded: vec![0.0; CONVERT_CHUNK * usize::from(format.channels)].into_boxed_slice(),
            stereo: vec![0.0; CONVERT_CHUNK * 2].into_boxed_slice(),
            resampled: vec![0.0; resampled_frames * 2].into_boxed_slice(),
        })
    }

    /// Converts one captured packet of `frames` frames and writes it to the
    /// ring. `data` holds the packet in the converter's format; only the
    /// whole frames it holds are used. A silent packet (Windows'
    /// `AUDCLNT_BUFFERFLAGS_SILENT`) writes `frames` frames of silence,
    /// whatever `data` holds; it still runs through the equalizer, so the
    /// filters ring out as they would on any quiet passage. The AirPlay
    /// sender gets the packet as audio that arrived now. However many writes
    /// the packet takes, the ring counts it as one block (S206), stamped
    /// with the time it closes on the core's clock (S207), so the outputs
    /// can follow the capture's clock.
    pub fn push(&mut self, data: &[u8], frames: usize, silent: bool) {
        let arrived = Instant::now();
        let block = self.format.block_align();
        let frames = if silent {
            frames
        } else {
            frames.min(data.len() / block)
        };
        let channels = usize::from(self.format.channels);
        let mut done = 0;
        while done < frames {
            let n = (frames - done).min(CONVERT_CHUNK);
            if silent {
                self.stereo[..n * 2].fill(0.0);
            } else {
                let bytes = &data[done * block..(done + n) * block];
                decode(self.format.sample, bytes, &mut self.decoded[..n * channels]);
                input_to_stereo(
                    &self.decoded[..n * channels],
                    channels,
                    &mut self.stereo[..n * 2],
                );
            }
            self.write_stereo(n, arrived);
            done += n;
        }
        self.writer.end_block_at(clock::now_ns());
    }

    /// Writes the first `frames` frames of `stereo`, resampled if needed and
    /// equalized, as part of the packet's block, and hands them to the
    /// AirPlay sender as audio that arrived at `arrived`.
    fn write_stereo(&mut self, frames: usize, arrived: Instant) {
        let CaptureConverter {
            writer,
            equalizer,
            airplay,
            resampler,
            stereo,
            resampled,
            ..
        } = self;
        match resampler {
            None => {
                let block = &mut stereo[..frames * 2];
                equalizer.process(block);
                writer.write_part(block, frames);
                if let Some(airplay) = airplay {
                    airplay.push(block, frames, arrived);
                }
            }
            Some(resampler) => {
                let input = &stereo[..frames * 2];
                let mut consumed = 0;
                while consumed < frames {
                    let (used, produced) = resampler.process(&input[consumed * 2..], resampled);
                    let block = &mut resampled[..produced * 2];
                    equalizer.process(block);
                    writer.write_part(block, produced);
                    if let Some(airplay) = airplay.as_mut() {
                        airplay.push(block, produced, arrived);
                    }
                    consumed += used;
                    if used == 0 && produced == 0 {
                        break;
                    }
                }
            }
        }
    }
}

/// 100 ns units per second: the unit of packet positions.
const HNS_PER_SECOND: u64 = 10_000_000;

/// Packets whose positions are further apart than this, 5 ms, have a gap
/// between them; closer, it is the positions' own jitter.
const GAP_JUMP_HNS: u64 = 50_000;

/// With no packet for this long after the last frame's end, 25 ms (more than
/// a late packet but inside the readers' 50 ms dead band, S50), the capture
/// is in a gap.
const IDLE_AFTER_HNS: u64 = 250_000;

/// In a gap, silence is written up to this long before now, 10 ms, so a
/// packet that starts the audio again rarely finds its first frames filled
/// already.
const IDLE_LAG_HNS: u64 = 100_000;

/// The most silence one gap writes, 21 s: more than the longest delay (S77),
/// so after a longer stall (a PC that slept) every reader finds silence.
const MAX_FILL_SECONDS: u64 = 21;

/// Keeps the ring in real time across a loopback gap.
///
/// Process loopback delivers nothing while the captured audio is silent.
/// Without this the ring would stop while the outputs read on: an output
/// lined up with AirPlay speakers, 1.75 s behind, would go quiet a few
/// milliseconds into the pause (S53) and replay those 1.75 s when the audio
/// came back. So a packet whose position jumps past the end of the last one
/// (or that Windows marks as a discontinuity) gets silence for the time
/// between them, and while nothing comes, each wake of the capture loop
/// writes silence up to shortly before now. A packet that starts inside
/// silence written that way skips the frames already filled. Times are in
/// 100 ns units of the performance counter, as
/// `IAudioCaptureClient::GetBuffer` gives a packet's position. Never
/// allocates, locks or blocks.
pub struct GapFill {
    rate: u32,
    /// When the last packet that set the pace started; `None` before the
    /// first packet. The frames written count from here, so filling a long
    /// gap in many small steps never adds up rounding.
    anchor: Option<u64>,
    /// Frames written from `anchor` on.
    written: u64,
    /// Whether the frames written so far end with silence written while
    /// nothing came, and no packet has reached past it yet.
    filled: bool,
}

impl GapFill {
    /// A tracker for a capture at `rate`, before its first packet.
    pub fn new(rate: u32) -> GapFill {
        GapFill {
            rate,
            anchor: None,
            written: 0,
            filled: false,
        }
    }

    /// When the frame after the last one written is due.
    fn end(&self) -> Option<u64> {
        self.anchor
            .map(|anchor| anchor.saturating_add(self.length(self.written)))
    }

    /// The pace starts again from a packet of `frames` frames at `start`.
    fn rebase(&mut self, start: u64, frames: usize) {
        self.anchor = Some(start);
        self.written = frames as u64;
    }

    /// The frames in `hns` at the capture rate, rounded down.
    fn frames_in(&self, hns: u64) -> u64 {
        (u128::from(hns) * u128::from(self.rate) / u128::from(HNS_PER_SECOND)) as u64
    }

    /// How long `frames` frames last, rounded down.
    fn length(&self, frames: u64) -> u64 {
        (u128::from(frames) * u128::from(HNS_PER_SECOND) / u128::from(self.rate.max(1))) as u64
    }

    fn max_fill(&self) -> u64 {
        MAX_FILL_SECONDS * u64::from(self.rate)
    }

    /// A packet of `frames` frames whose first frame was captured at
    /// `start`. Returns the silent frames to write before it, and how many
    /// of its first frames to skip. Each packet's own position counts, so
    /// the device clock's drift never adds up to a gap.
    pub fn packet(&mut self, start: u64, frames: usize, discontinuity: bool) -> (usize, usize) {
        let packet_end = start.saturating_add(self.length(frames as u64));
        let filled = std::mem::take(&mut self.filled);
        let Some(end) = self.end() else {
            self.rebase(start, frames);
            return (0, 0);
        };
        if start >= end {
            let gap = start - end;
            let fill = if filled || discontinuity || gap > GAP_JUMP_HNS {
                self.frames_in(gap).min(self.max_fill())
            } else {
                0
            };
            self.rebase(start, frames);
            return (fill as usize, 0);
        }
        if filled {
            // It starts inside the silence written while nothing came: its
            // frames up to the fill's end are there already. One that ends
            // inside the fill too is skipped whole, and the next late packet
            // still finds the fill.
            let skip = (self.frames_in(end - start) as usize).min(frames);
            if packet_end > end {
                self.rebase(start, frames);
            } else {
                self.filled = true;
            }
            return (0, skip);
        }
        self.rebase(start, frames);
        (0, 0)
    }

    /// No packet came, and it is `now`. Returns the silent frames to write:
    /// none until the capture is in a gap, then enough to reach shortly
    /// before now.
    pub fn idle(&mut self, now: u64) -> usize {
        let (Some(anchor), Some(end)) = (self.anchor, self.end()) else {
            return 0;
        };
        if !self.filled && now < end.saturating_add(IDLE_AFTER_HNS) {
            return 0;
        }
        let target = now.saturating_sub(IDLE_LAG_HNS);
        if target <= end {
            return 0;
        }
        // Counted from the anchor, so no step's rounding is ever lost.
        let wanted = self.frames_in(target - anchor).saturating_sub(self.written);
        if wanted == 0 {
            return 0;
        }
        let frames = wanted.min(self.max_fill());
        if frames < wanted {
            // A longer stall: the pace starts again from now.
            self.anchor = Some(target);
            self.written = 0;
        } else {
            self.written += frames;
        }
        self.filled = true;
        frames as usize
    }
}

/// Decodes little-endian samples into floats in -1.0..=1.0.
fn decode(sample: SampleType, bytes: &[u8], out: &mut [f32]) {
    match sample {
        SampleType::F32 => {
            for (value, raw) in out.iter_mut().zip(bytes.as_chunks::<4>().0) {
                *value = f32::from_le_bytes(*raw);
            }
        }
        SampleType::I16 => {
            for (value, raw) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
                *value = f32::from(i16::from_le_bytes(*raw)) / 32_768.0;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use audiosync_airplay::clock::SenderClock;
    use audiosync_airplay::stream::{Stream, StreamConfig};
    use audiosync_core::eq::{EqSettings, equalizer};
    use audiosync_core::reader::{Correction, Reader, tolerance_frames};
    use audiosync_core::ring::Ring;

    use super::*;
    use crate::audio::airplay::FeedSlot;
    use crate::audio::outputs::EndpointProperties;
    use crate::audio::process::tests::{desktop, entry};

    /// AudioSync's process ID in [`desktop`].
    const OWN_PID: u32 = 400;

    fn app(root_pid: u32) -> SourceId {
        SourceId::App { root_pid }
    }

    fn input_reading(
        index: u32,
        id: &str,
        name: Option<&str>,
        enumerator: &str,
    ) -> EndpointReading {
        EndpointReading {
            index,
            id: Ok(id.to_owned()),
            properties: Ok(EndpointProperties {
                name: name.map(str::to_owned),
                enumerator: Some(enumerator.to_owned()),
                ..EndpointProperties::default()
            }),
        }
    }

    #[test]
    fn input_devices_are_offered_by_name_without_hands_free_or_unreadable_ones() {
        let readings = vec![
            input_reading(0, "usb", Some("Microphone (USB Audio)"), "USB"),
            input_reading(
                1,
                "hf",
                Some("Headset (Example Buds Hands-Free)"),
                "BTHHFENUM",
            ),
            input_reading(2, "line", Some("Line In (Realtek Audio)"), "HDAUDIO"),
            input_reading(3, "nameless", None, "SWD"),
            input_reading(4, "blank", Some("  "), "SWD"),
            EndpointReading {
                index: 5,
                id: Err("IMMDevice::GetId failed (0x80070490)".to_owned()),
                properties: Ok(EndpointProperties::default()),
            },
            EndpointReading {
                index: 6,
                id: Ok("array".to_owned()),
                properties: Err("IMMDevice::OpenPropertyStore failed".to_owned()),
            },
        ];
        let offered: Vec<(SourceId, String)> = input_sources(readings)
            .into_iter()
            .map(|source| {
                assert!(source.executable.is_empty());
                (source.id, source.name)
            })
            .collect();
        let input = |id: &str, name: &str| {
            (
                SourceId::Input {
                    endpoint_id: id.to_owned(),
                },
                name.to_owned(),
            )
        };
        assert_eq!(
            offered,
            [
                input("array", "array"),
                input("blank", "blank"),
                input("line", "Line In (Realtek Audio)"),
                input("usb", "Microphone (USB Audio)"),
                input("nameless", "nameless"),
            ]
        );
    }

    #[test]
    fn an_input_device_is_captured_from_its_endpoint_not_by_process_loopback() {
        let source = SourceId::Input {
            endpoint_id: "usb".to_owned(),
        };
        assert_eq!(
            decide_capture(&source, OWN_PID, &ProcessTable::default()),
            Ok(CaptureTarget::Input {
                endpoint_id: "usb".to_owned()
            })
        );
        assert_eq!(
            decide_activation(&source, OWN_PID, &desktop()),
            Err(BackendError::NotAnApp)
        );
        assert_eq!(
            decide_capture(&SourceId::WholeSystem, OWN_PID, &desktop()),
            Ok(CaptureTarget::Loopback(Activation {
                mode: LoopbackMode::ExcludeTree,
                target_pid: OWN_PID,
            }))
        );
    }

    #[test]
    fn the_whole_system_excludes_audiosyncs_own_tree() {
        let activation = decide_activation(&SourceId::WholeSystem, OWN_PID, &desktop());
        assert_eq!(
            activation,
            Ok(Activation {
                mode: LoopbackMode::ExcludeTree,
                target_pid: OWN_PID,
            })
        );
    }

    #[test]
    fn the_whole_system_needs_no_process_snapshot() {
        let activation = decide_activation(&SourceId::WholeSystem, 7, &ProcessTable::default());
        assert_eq!(
            activation,
            Ok(Activation {
                mode: LoopbackMode::ExcludeTree,
                target_pid: 7,
            })
        );
    }

    #[test]
    fn an_app_includes_its_own_tree() {
        assert_eq!(
            decide_activation(&app(200), OWN_PID, &desktop()),
            Ok(Activation {
                mode: LoopbackMode::IncludeTree,
                target_pid: 200,
            })
        );
    }

    #[test]
    fn audiosync_picked_by_mistake_is_refused() {
        assert_eq!(
            decide_activation(&app(OWN_PID), OWN_PID, &desktop()),
            Err(BackendError::OwnProcess)
        );
    }

    #[test]
    fn audiosync_is_refused_even_when_the_snapshot_misses_it() {
        assert_eq!(
            decide_activation(&app(OWN_PID), OWN_PID, &ProcessTable::default()),
            Err(BackendError::OwnProcess)
        );
    }

    #[test]
    fn a_process_audiosync_started_is_refused() {
        assert_eq!(
            decide_activation(&app(401), OWN_PID, &desktop()),
            Err(BackendError::OwnProcess)
        );
    }

    #[test]
    fn the_app_that_started_audiosync_is_refused() {
        let table = desktop();
        assert_eq!(
            decide_activation(&app(300), OWN_PID, &table),
            Err(BackendError::ContainsAudioSync)
        );
        assert_eq!(
            decide_activation(&app(100), OWN_PID, &table),
            Err(BackendError::ContainsAudioSync),
            "explorer started the terminal that started AudioSync"
        );
    }

    #[test]
    fn a_vanished_app_is_gone() {
        assert_eq!(
            decide_activation(&app(4242), OWN_PID, &desktop()),
            Err(BackendError::SourceGone)
        );
    }

    #[test]
    fn the_idle_and_system_processes_are_not_apps() {
        let table = desktop();
        assert_eq!(
            decide_activation(&app(IDLE_PID), OWN_PID, &table),
            Err(BackendError::NotAnApp)
        );
        assert_eq!(
            decide_activation(&app(SYSTEM_PID), OWN_PID, &table),
            Err(BackendError::NotAnApp)
        );
    }

    #[test]
    fn an_old_parent_id_reused_by_audiosync_does_not_block_an_app() {
        // The app's creator exited and AudioSync, started later, got its ID.
        let table = ProcessTable::new([
            entry(50, 400, Some(5), "player.exe"),
            entry(400, 1, Some(9), "audiosync.exe"),
        ]);
        assert_eq!(
            decide_activation(&app(50), 400, &table),
            Ok(Activation {
                mode: LoopbackMode::IncludeTree,
                target_pid: 50,
            })
        );
    }

    /// Explorer (100) started a terminal (300) that started AudioSync (400),
    /// which started a helper (401). Explorer's creator (90) exited long ago,
    /// and a player that Explorer started later got its ID; `player_created`
    /// is when, if known.
    fn reused_explorer_parent(player_created: Option<u64>) -> ProcessTable {
        ProcessTable::new([
            entry(90, 100, player_created, "player.exe"),
            entry(100, 90, Some(10), "explorer.exe"),
            entry(300, 100, Some(30), "terminal.exe"),
            entry(400, 300, Some(40), "audiosync.exe"),
            entry(401, 400, Some(41), "helper.exe"),
        ])
    }

    #[test]
    fn an_app_that_got_the_id_of_an_exited_ancestor_can_be_captured() {
        let table = reused_explorer_parent(Some(60));
        assert_eq!(
            decide_activation(&app(90), OWN_PID, &table),
            Ok(Activation {
                mode: LoopbackMode::IncludeTree,
                target_pid: 90,
            })
        );
        let sources = source_list(&table, &[90], OWN_PID);
        assert_eq!(sources.len(), 2, "offered: {sources:?}");
        assert_eq!(sources[1].id, app(90));
    }

    #[test]
    fn audiosyncs_real_ancestors_and_children_stay_refused() {
        let table = reused_explorer_parent(Some(60));
        for (pid, refusal) in [
            (300, BackendError::ContainsAudioSync),
            (100, BackendError::ContainsAudioSync),
            (401, BackendError::OwnProcess),
            (4242, BackendError::SourceGone),
        ] {
            assert_eq!(
                decide_activation(&app(pid), OWN_PID, &table),
                Err(refusal),
                "{pid}"
            );
        }
    }

    #[test]
    fn an_app_that_may_be_an_ancestor_is_refused() {
        // The player can't be opened, so whether it started after Explorer
        // is unknown.
        let table = reused_explorer_parent(None);
        assert_eq!(
            decide_activation(&app(90), OWN_PID, &table),
            Err(BackendError::ContainsAudioSync)
        );
        assert_eq!(source_list(&table, &[90], OWN_PID).len(), 1);
    }

    #[test]
    fn an_app_that_got_the_id_of_audiosyncs_exited_launcher_can_be_captured() {
        // AudioSync's launcher (300) exited, and a player started after
        // AudioSync got its ID.
        let table = ProcessTable::new([
            entry(100, 90, Some(10), "explorer.exe"),
            entry(300, 100, Some(50), "player.exe"),
            entry(400, 300, Some(40), "audiosync.exe"),
        ]);
        assert_eq!(
            decide_activation(&app(300), 400, &table),
            Ok(Activation {
                mode: LoopbackMode::IncludeTree,
                target_pid: 300,
            })
        );
    }

    #[test]
    fn no_app_is_captured_while_audiosync_is_missing_from_the_snapshot() {
        let table = ProcessTable::new([entry(200, 100, Some(20), "player.exe")]);
        assert_eq!(
            decide_activation(&app(200), OWN_PID, &table),
            Err(BackendError::ContainsAudioSync)
        );
        assert_eq!(source_list(&table, &[200], OWN_PID).len(), 1);
        assert!(decide_activation(&SourceId::WholeSystem, OWN_PID, &table).is_ok());
    }

    #[test]
    fn display_names_drop_the_extension_and_capitalize() {
        assert_eq!(display_name("spotify.exe"), "Spotify");
        assert_eq!(display_name("Browser.EXE"), "Browser");
        assert_eq!(display_name("vlc"), "Vlc");
        assert_eq!(display_name(".exe"), ".exe");
        assert_eq!(display_name(""), "");
        assert_eq!(display_name("é.exe"), "É");
    }

    #[test]
    fn the_source_list_starts_with_the_whole_system() {
        let sources = source_list(&desktop(), &[], OWN_PID);
        assert_eq!(
            sources,
            vec![SourceInfo {
                id: SourceId::WholeSystem,
                name: WHOLE_SYSTEM_NAME.to_owned(),
                executable: String::new(),
            }]
        );
    }

    #[test]
    fn an_app_with_several_audio_processes_is_listed_once_by_its_top_process() {
        let sources = source_list(&desktop(), &[202, 201, 200], OWN_PID);
        assert_eq!(sources.len(), 2);
        assert_eq!(sources[1].id, app(200));
        assert_eq!(sources[1].name, "Browser");
        assert_eq!(sources[1].executable, "browser.exe");
    }

    #[test]
    fn audiosync_its_helpers_and_its_ancestors_are_not_offered() {
        let sources = source_list(&desktop(), &[OWN_PID, 401, 300, 100], OWN_PID);
        assert_eq!(sources.len(), 1, "{sources:?}");
    }

    #[test]
    fn sessions_of_processes_that_quit_are_skipped() {
        let sources = source_list(&desktop(), &[9999, 0], OWN_PID);
        assert_eq!(sources.len(), 1);
    }

    #[test]
    fn apps_are_sorted_by_name() {
        let table = ProcessTable::new([
            entry(10, 1, None, "zebra.exe"),
            entry(20, 1, None, "Alpha.exe"),
            entry(30, 1, None, "mid.exe"),
            entry(99, 1, None, "audiosync.exe"),
        ]);
        let names: Vec<String> = source_list(&table, &[10, 20, 30], 99)
            .into_iter()
            .map(|source| source.name)
            .collect();
        assert_eq!(names, ["All System Audio", "Alpha", "Mid", "Zebra"]);
    }

    #[test]
    fn a_running_app_is_found_without_an_audio_session() {
        let browser = SourceInfo {
            id: app(200),
            name: "Browser".to_owned(),
            executable: "browser.exe".to_owned(),
        };
        let table = desktop();
        assert_eq!(
            running_app(&table, 200, "browser.exe"),
            Some(browser.clone())
        );
        assert_eq!(
            running_app(&table, 202, "browser.exe"),
            Some(browser.clone()),
            "a helper leads to the app's top process"
        );
        assert_eq!(
            running_app(&table, 4242, "browser.exe"),
            Some(browser),
            "an app that started again has a new process ID"
        );
    }

    #[test]
    fn a_process_id_that_another_program_got_is_not_the_app() {
        let table = desktop();
        assert_eq!(running_app(&table, 300, "player.exe"), None);
        assert_eq!(running_app(&table, 4242, "player.exe"), None);
    }

    #[test]
    fn the_first_capture_attempt_needs_no_conversion() {
        let first = CAPTURE_ATTEMPTS[0];
        assert_eq!(first.format.sample, SampleType::F32);
        assert_eq!(first.format.rate, SESSION_RATE);
        assert_eq!(first.format.channels, 2);
    }

    #[test]
    fn every_capture_attempt_is_stereo_at_a_supported_rate() {
        for attempt in CAPTURE_ATTEMPTS {
            assert_eq!(attempt.format.channels, 2);
            assert!(is_supported_rate(attempt.format.rate));
        }
    }

    #[test]
    fn block_align_counts_bytes_per_frame() {
        let format = StreamFormat {
            sample: SampleType::I16,
            rate: 44_100,
            channels: 2,
        };
        assert_eq!(format.block_align(), 4);
        assert_eq!(CAPTURE_ATTEMPTS[0].format.block_align(), 8);
    }

    fn session_ring() -> (Arc<Ring>, RingWriter) {
        let ring = Ring::for_rate(SESSION_RATE).expect("48 kHz is supported");
        let writer = ring.writer().expect("a new ring has a writer");
        (ring, writer)
    }

    /// Frames of silence [`session_ring_after_lead_in`] writes before the
    /// audio under test; see [`newest`].
    const LEAD_IN: u64 = 1;

    /// A session ring whose writer has already written [`LEAD_IN`] frames of
    /// silence, straight to the ring, so a converter made with it sees only
    /// the packets a test pushes.
    fn session_ring_after_lead_in() -> (Arc<Ring>, RingWriter) {
        let (ring, mut writer) = session_ring();
        writer.write(&[0.0; 2 * LEAD_IN as usize], LEAD_IN as usize);
        (ring, writer)
    }

    /// A converter whose equalizer is off, as it starts.
    fn new_converter(
        format: StreamFormat,
        writer: RingWriter,
    ) -> Result<CaptureConverter, BackendError> {
        let (_control, processor) = equalizer();
        CaptureConverter::new(format, writer, processor, None)
    }

    /// Settings that only raise the level, by `preamp_db`: flat bands pass
    /// audio unchanged, so the preamp alone shows in the output.
    fn preamp_only(preamp_db: f64) -> EqSettings {
        EqSettings {
            enabled: true,
            preamp_db,
            ..EqSettings::default()
        }
    }

    fn f32_bytes(samples: &[f32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    /// Reads the newest `frames` frames of the ring, through a new reader at
    /// a delay of `frames`. A new reader primes only on a read that finds
    /// more frames written than its delay, at `W - De` (S45), so it never
    /// reads frame 0: the ring must hold more than `frames`, and a test that
    /// reads back everything it wrote starts from
    /// [`session_ring_after_lead_in`].
    fn newest(ring: &Arc<Ring>, frames: usize) -> Vec<f32> {
        let mut reader = Reader::new(Arc::clone(ring), SESSION_RATE);
        let mut out = vec![0.0; frames * 2];
        let report = reader.read(&mut out, frames, frames as i64, 0);
        assert_eq!(
            (report.correction, report.copied),
            (Correction::Prime, frames),
            "the read primes and copies all {frames} frames, which needs more than \
             {frames} written ({} are)",
            ring.frames_written()
        );
        out
    }

    #[test]
    fn float_stereo_at_the_session_rate_is_written_unchanged() {
        let (ring, writer) = session_ring_after_lead_in();
        let mut converter = new_converter(CAPTURE_ATTEMPTS[0].format, writer).unwrap();
        let samples: Vec<f32> = (0..3000).map(|i| (i as f32 / 3000.0) - 0.5).collect();
        converter.push(&f32_bytes(&samples), 1500, false);
        assert_eq!(ring.frames_written(), LEAD_IN + 1500);
        assert_eq!(newest(&ring, 1500), samples);
    }

    #[test]
    fn sixteen_bit_samples_are_scaled_to_float() {
        let (ring, writer) = session_ring_after_lead_in();
        let format = StreamFormat {
            sample: SampleType::I16,
            rate: SESSION_RATE,
            channels: 2,
        };
        let mut converter = new_converter(format, writer).unwrap();
        let samples: [i16; 4] = [i16::MIN, 16_384, 0, -16_384];
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        converter.push(&bytes, 2, false);
        assert_eq!(newest(&ring, 2), [-1.0, 0.5, 0.0, -0.5]);
    }

    #[test]
    fn a_silent_packet_writes_silence_whatever_its_data() {
        let (ring, writer) = session_ring_after_lead_in();
        let mut converter = new_converter(CAPTURE_ATTEMPTS[0].format, writer).unwrap();
        converter.push(&f32_bytes(&[0.25; 8]), 4, false);
        converter.push(&f32_bytes(&[0.75; 8]), 4, true);
        assert_eq!(ring.frames_written(), LEAD_IN + 8);
        let out = newest(&ring, 8);
        assert_eq!(&out[..8], &[0.25; 8]);
        assert_eq!(&out[8..], &[0.0; 8]);
    }

    #[test]
    fn a_mono_capture_feeds_both_channels() {
        let (ring, writer) = session_ring_after_lead_in();
        let format = StreamFormat {
            sample: SampleType::F32,
            rate: SESSION_RATE,
            channels: 1,
        };
        let mut converter = new_converter(format, writer).unwrap();
        converter.push(&f32_bytes(&[0.1, 0.2, 0.3]), 3, false);
        assert_eq!(newest(&ring, 3), [0.1, 0.1, 0.2, 0.2, 0.3, 0.3]);
    }

    #[test]
    fn a_short_packet_writes_only_its_whole_frames() {
        let (ring, writer) = session_ring();
        let mut converter = new_converter(CAPTURE_ATTEMPTS[0].format, writer).unwrap();
        let mut bytes = f32_bytes(&[0.5; 6]);
        bytes.truncate(bytes.len() - 1);
        converter.push(&bytes, 10, false);
        assert_eq!(ring.frames_written(), 2);
    }

    #[test]
    fn a_long_packet_is_converted_in_chunks() {
        let (ring, writer) = session_ring_after_lead_in();
        let mut converter = new_converter(CAPTURE_ATTEMPTS[0].format, writer).unwrap();
        let frames = CONVERT_CHUNK * 3 + 17;
        let samples: Vec<f32> = (0..frames * 2)
            .map(|i| (i % 1000) as f32 / 1000.0)
            .collect();
        converter.push(&f32_bytes(&samples), frames, false);
        assert_eq!(ring.frames_written(), LEAD_IN + frames as u64);
        assert_eq!(newest(&ring, frames), samples);
    }

    #[test]
    fn a_44_1_khz_capture_is_resampled_to_the_session_rate() {
        let (ring, writer) = session_ring();
        let format = CAPTURE_ATTEMPTS[3].format;
        assert_eq!(format.rate, 44_100);
        let mut converter = new_converter(format, writer).unwrap();
        // One second of a 1 kHz tone at half scale, in 441-frame packets.
        let mut fed = 0usize;
        for _ in 0..100 {
            let bytes: Vec<u8> = (fed..fed + 441)
                .flat_map(|i| {
                    let phase = i as f64 * 1000.0 / 44_100.0;
                    let value = (0.5 * (phase * std::f64::consts::TAU).sin() * 32_767.0) as i16;
                    let [lo, hi] = value.to_le_bytes();
                    [lo, hi, lo, hi]
                })
                .collect();
            converter.push(&bytes, 441, false);
            fed += 441;
        }
        let written = ring.frames_written() as i64;
        assert!(
            (written - 48_000).abs() <= 2,
            "wrote {written} frames for one second"
        );
        let out = newest(&ring, 24_000);
        let peak = out.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        assert!((0.45..0.55).contains(&peak), "peak {peak}");
        assert!(out.iter().all(|s| s.is_finite()));
    }

    /// The outputs' lead covers the capture's blocks (S206): a packet the
    /// converter writes in pieces, in conversion chunks or as the resampler
    /// hands them out, is one block, and a short packet after it (what a gap
    /// fill left of it) leaves the larger block in view.
    #[test]
    fn a_packet_written_in_pieces_is_one_block_for_the_outputs_lead() {
        for format in [CAPTURE_ATTEMPTS[0].format, CAPTURE_ATTEMPTS[3].format] {
            let (ring, writer) = session_ring();
            let mut converter = new_converter(format, writer).unwrap();
            let align = format.block_align();
            // More than two conversion chunks.
            let frames = CONVERT_CHUNK * 2 + 157;
            converter.push(&vec![0u8; frames * align], frames, false);
            let packet = usize::try_from(ring.frames_written()).unwrap();
            assert!(packet > CONVERT_CHUNK * 2, "{} Hz: {packet}", format.rate);
            assert_eq!(ring.block_frames(), packet, "{} Hz", format.rate);
            converter.push(&vec![0u8; 37 * align], 37, false);
            assert_eq!(ring.block_frames(), packet, "{} Hz", format.rate);
        }
    }

    /// Every packet's block, a gap fill's silence included, is stamped with
    /// W and the time it closed on the core's clock (S207); a packet that
    /// wrote nothing leaves the newest stamp as it was.
    // covers: S207
    #[test]
    fn each_packet_and_gap_fill_is_stamped_as_its_block_closes() {
        let (ring, writer) = session_ring();
        let mut converter = new_converter(CAPTURE_ATTEMPTS[0].format, writer).unwrap();
        assert_eq!(ring.stamp(), None);
        let before = clock::now_ns();
        converter.push(&f32_bytes(&[0.25; 960]), 480, false);
        let first = ring.stamp().expect("the packet's block is stamped");
        assert_eq!(first.written, 480);
        assert!(first.time_ns >= before && first.time_ns <= clock::now_ns());
        converter.push(&[], 480, true);
        let fill = ring.stamp().expect("the gap fill's block is stamped");
        assert_eq!(fill.written, 960);
        assert!(fill.time_ns >= first.time_ns);
        converter.push(&[], 0, false);
        assert_eq!(ring.stamp(), Some(fill));
    }

    #[test]
    fn a_rate_outside_the_supported_range_is_refused() {
        let (_ring, writer) = session_ring();
        let format = StreamFormat {
            sample: SampleType::F32,
            rate: 4_000,
            channels: 2,
        };
        assert!(matches!(
            new_converter(format, writer),
            Err(BackendError::UnsupportedFormat)
        ));
    }

    #[test]
    fn a_format_without_channels_is_refused() {
        let (_ring, writer) = session_ring();
        let format = StreamFormat {
            sample: SampleType::F32,
            rate: SESSION_RATE,
            channels: 0,
        };
        assert!(matches!(
            new_converter(format, writer),
            Err(BackendError::UnsupportedFormat)
        ));
    }

    #[test]
    fn the_equalizer_applies_from_the_next_packet() {
        let (ring, writer) = session_ring_after_lead_in();
        let (mut control, processor) = equalizer();
        let mut converter =
            CaptureConverter::new(CAPTURE_ATTEMPTS[0].format, writer, processor, None).unwrap();
        converter.push(&f32_bytes(&[0.25; 8]), 4, false);
        assert_eq!(newest(&ring, 4), [0.25; 8], "off, audio passes unchanged");

        control.set_settings(preamp_only(6.0));
        converter.push(&f32_bytes(&[0.25; 8]), 4, false);
        let raised = 0.25 * 10f32.powf(6.0 / 20.0);
        for sample in newest(&ring, 4) {
            assert!((sample - raised).abs() < 1e-4, "{sample} is {raised}");
        }
    }

    #[test]
    fn a_resampled_capture_is_equalized_at_the_session_rate() {
        let (ring, writer) = session_ring();
        let (mut control, processor) = equalizer();
        control.set_settings(preamp_only(-6.0));
        let format = CAPTURE_ATTEMPTS[3].format;
        let mut converter = CaptureConverter::new(format, writer, processor, None).unwrap();
        // A tenth of a second of a 1 kHz tone at half scale.
        let bytes: Vec<u8> = (0..4410)
            .flat_map(|i| {
                let phase = f64::from(i) * 1000.0 / 44_100.0;
                let value = (0.5 * (phase * std::f64::consts::TAU).sin() * 32_767.0) as i16;
                let [lo, hi] = value.to_le_bytes();
                [lo, hi, lo, hi]
            })
            .collect();
        converter.push(&bytes, 4410, false);
        let out = newest(&ring, 2400);
        let peak = out.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
        assert!((0.22..0.28).contains(&peak), "peak {peak}");
    }

    /// 100 ns units per millisecond, as packet positions count.
    const HNS_PER_MS: u64 = 10_000;

    #[test]
    fn a_loopback_gap_keeps_the_ring_in_real_time_so_nothing_from_before_it_plays_after_it() {
        let (ring, writer) = session_ring();
        let (_control, processor) = equalizer();
        let mut converter =
            CaptureConverter::new(CAPTURE_ATTEMPTS[0].format, writer, processor, None).unwrap();
        let mut gaps = GapFill::new(SESSION_RATE);
        // An output lined up with an AirPlay speaker: L = 1750 ms.
        let mut reader = Reader::new(Arc::clone(&ring), SESSION_RATE);
        let (delay, tolerance) = (84_000, tolerance_frames(SESSION_RATE));
        let mut out = vec![0.0_f32; 480 * 2];
        let mut played = Vec::new();
        // 10 ms steps: 3 s of audio, a 2 s gap in which Windows delivers
        // nothing and the loop wakes once a period, then 3 s of other audio.
        for step in 0..800_u64 {
            let start = step * 10 * HNS_PER_MS;
            let level = match step {
                0..300 => Some(0.5),
                300..500 => None,
                _ => Some(0.25),
            };
            match level {
                Some(level) => {
                    let (fill, skip) = gaps.packet(start, 480, false);
                    if fill > 0 {
                        converter.push(&[], fill, true);
                    }
                    let frames = 480 - skip;
                    converter.push(&f32_bytes(&vec![level; frames * 2]), frames, false);
                }
                None => {
                    let fill = gaps.idle(start + 10 * HNS_PER_MS);
                    if fill > 0 {
                        converter.push(&[], fill, true);
                    }
                }
            }
            reader.read(&mut out, 480, delay, tolerance);
            played.push(out.clone());
        }
        for (step, block) in played.iter().enumerate().take(470).skip(300) {
            assert!(
                block.iter().all(|s| *s == 0.5),
                "step {step}: the output plays on until the gap reaches it"
            );
        }
        for (step, block) in played.iter().enumerate().skip(480) {
            assert!(
                !block.contains(&0.5),
                "step {step}: audio from before the gap plays again"
            );
        }
        assert!(
            played[700].iter().all(|s| *s == 0.25),
            "the audio after the gap plays 1.75 s after it came"
        );
    }

    #[test]
    fn contiguous_packets_need_no_silence_whatever_their_jitter() {
        let mut gaps = GapFill::new(SESSION_RATE);
        let mut start = 0;
        for jitter in [0_i64, 3_000, -2_000, 20_000, -40_000, 0] {
            let at = (start as i64 + jitter) as u64;
            assert_eq!(gaps.packet(at, 480, false), (0, 0), "{jitter}");
            start += 10 * HNS_PER_MS;
        }
        // A late packet's wake isn't a gap yet.
        assert_eq!(gaps.idle(start + 20 * HNS_PER_MS), 0);
    }

    #[test]
    fn a_jump_or_a_discontinuity_gets_silence_for_the_time_between() {
        let mut gaps = GapFill::new(SESSION_RATE);
        gaps.packet(0, 480, false);
        // 10 ms later than the last packet's end: 480 frames of silence.
        assert_eq!(gaps.packet(20 * HNS_PER_MS, 480, false), (480, 0));
        // 2 ms, marked as a discontinuity: 96 frames.
        assert_eq!(gaps.packet(32 * HNS_PER_MS, 480, true), (96, 0));
        // A PC that slept: at most 21 s.
        assert_eq!(
            gaps.packet(3_600_000 * HNS_PER_MS, 480, false),
            (21 * 48_000, 0)
        );
    }

    #[test]
    fn a_packet_inside_the_silence_written_skips_the_frames_filled() {
        let mut gaps = GapFill::new(SESSION_RATE);
        gaps.packet(0, 480, false);
        // Nothing for 50 ms: silence up to 40 ms.
        assert_eq!(gaps.idle(50 * HNS_PER_MS), 1_440);
        // The audio comes back with a packet that started at 35 ms.
        assert_eq!(gaps.packet(35 * HNS_PER_MS, 480, false), (0, 240));
        // The next one follows on.
        assert_eq!(gaps.packet(45 * HNS_PER_MS, 480, false), (0, 0));
    }

    #[test]
    fn late_packets_inside_one_fill_are_never_written_twice() {
        let mut gaps = GapFill::new(SESSION_RATE);
        gaps.packet(0, 480, false);
        // Silence up to 30 ms.
        assert_eq!(gaps.idle(40 * HNS_PER_MS), 960);
        // Two late packets that both lie inside it: neither is written.
        assert_eq!(gaps.packet(10 * HNS_PER_MS, 480, false), (0, 480));
        assert_eq!(gaps.packet(20 * HNS_PER_MS, 480, false), (0, 480));
        // One that reaches past it is written from its end on.
        assert_eq!(gaps.packet(25 * HNS_PER_MS, 480, false), (0, 240));
        assert_eq!(gaps.packet(35 * HNS_PER_MS, 480, false), (0, 0));
    }

    #[test]
    fn an_hour_of_gap_fills_exactly_the_frames_of_an_hour() {
        let mut gaps = GapFill::new(SESSION_RATE);
        gaps.packet(0, 480, false);
        // Wakes about every 10 ms, at uneven times, for an hour.
        let mut written = 0_u64;
        let mut now = 0_u64;
        while now < 3_600_000 * HNS_PER_MS {
            now += 100_003;
            written += gaps.idle(now) as u64;
        }
        // Silence from the packet's end (10 ms) to 10 ms before the last
        // wake, at 48 kHz, to the frame.
        let expected = (now - 20 * HNS_PER_MS) * 48_000 / 10_000_000;
        assert!(
            written.abs_diff(expected) <= 1,
            "{written} frames written, {expected} due"
        );
    }

    #[test]
    fn the_airplay_sender_gets_the_equalized_audio_at_44_1_khz() {
        let clock = SenderClock::new(std::time::Instant::now());
        let (_stream, input) =
            Stream::new(StreamConfig::default(), clock, std::time::Instant::now());
        let slot = FeedSlot::new(input);
        let (ring, writer) = session_ring();
        let (_control, processor) = equalizer();
        let lease = slot.lease().expect("the feed is free");
        let mut converter =
            CaptureConverter::new(CAPTURE_ATTEMPTS[0].format, writer, processor, Some(lease))
                .unwrap();
        // One second at the session rate, in 10 ms packets.
        let packet = f32_bytes(&[0.25; 480 * 2]);
        for _ in 0..100 {
            converter.push(&packet, 480, false);
        }
        assert_eq!(ring.frames_written(), 48_000, "the ring gets it all");
        drop(converter);
        let lease = slot.lease().expect("the capture gave the feed back");
        let position = lease.position();
        assert!((44_000..=44_100).contains(&position), "{position} frames");
    }
}
