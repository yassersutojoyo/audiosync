//! What process loopback captures, which sources to offer, the formats a
//! capture asks Windows for, and the conversion of captured packets into the
//! shared ring.
//!
//! Process loopback captures the render streams of a process tree: either
//! only that tree (one app), or everything except it (the whole system,
//! without AudioSync). AudioSync must never capture its own output, or the
//! delayed copies would be captured again and echo (spec S10). So the whole
//! system excludes AudioSync's own tree, and an app is refused when its tree
//! contains AudioSync, or AudioSync's tree contains it. Whether the app's
//! tree contains AudioSync is decided on every recorded parent ID, reused or
//! not, since Windows doesn't document whether its tree walk checks creation
//! times: when unsure, AudioSync refuses rather than capture itself.

use audiosync_core::channels::input_to_stereo;
use audiosync_core::resample::Resampler;
use audiosync_core::ring::{RingWriter, is_supported_rate};

use super::SESSION_RATE;
use super::backend::{BackendError, SourceId, SourceInfo};
use super::process::{IDLE_PID, ProcessTable, SYSTEM_PID};

/// The name of the whole-system source in English, as the macOS app shows
/// it: for logs and the mock. The UI shows its own, translated label.
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

/// The capture for `source`, with AudioSync running as `own_pid` and the
/// running processes in `processes`.
///
/// The whole system excludes AudioSync's own tree. An app includes the tree
/// of its top process, unless that process is not an app, no longer runs,
/// is AudioSync or one of its descendants, or may be an ancestor of AudioSync
/// (which happens when the user picks the app that started AudioSync, such
/// as Explorer or a terminal, or an app that got the ID of a process in
/// AudioSync's ancestry after it exited).
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
    // Every recorded parent ID counts here, reused or not: Windows may walk
    // them without checking creation times.
    if processes.in_recorded_tree(own_pid, root_pid) {
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
/// [`SESSION_RATE`] and writes them to the ring.
///
/// All buffers are allocated by [`CaptureConverter::new`];
/// [`CaptureConverter::push`] never allocates, locks or blocks.
pub struct CaptureConverter {
    format: StreamFormat,
    writer: RingWriter,
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
    /// A converter for packets in `format`, writing through `writer`.
    pub fn new(format: StreamFormat, writer: RingWriter) -> Result<CaptureConverter, BackendError> {
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
            resampler,
            decoded: vec![0.0; CONVERT_CHUNK * usize::from(format.channels)].into_boxed_slice(),
            stereo: vec![0.0; CONVERT_CHUNK * 2].into_boxed_slice(),
            resampled: vec![0.0; resampled_frames * 2].into_boxed_slice(),
        })
    }

    /// The format packets arrive in.
    pub fn format(&self) -> StreamFormat {
        self.format
    }

    /// The ring writer.
    pub fn writer(&self) -> &RingWriter {
        &self.writer
    }

    /// Converts one captured packet of `frames` frames and writes it to the
    /// ring. `data` holds the packet in [`CaptureConverter::format`]; only the
    /// whole frames it holds are used. A silent packet (Windows'
    /// `AUDCLNT_BUFFERFLAGS_SILENT`) writes `frames` frames of silence,
    /// whatever `data` holds.
    pub fn push(&mut self, data: &[u8], frames: usize, silent: bool) {
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
            self.write_stereo(n);
            done += n;
        }
    }

    /// Writes the first `frames` frames of `stereo`, resampled if needed.
    fn write_stereo(&mut self, frames: usize) {
        let CaptureConverter {
            writer,
            resampler,
            stereo,
            resampled,
            ..
        } = self;
        let input = &stereo[..frames * 2];
        match resampler {
            None => writer.write(input, frames),
            Some(resampler) => {
                let mut consumed = 0;
                while consumed < frames {
                    let (used, produced) = resampler.process(&input[consumed * 2..], resampled);
                    writer.write(&resampled[..produced * 2], produced);
                    consumed += used;
                    if used == 0 && produced == 0 {
                        break;
                    }
                }
            }
        }
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

    use audiosync_core::reader::Reader;
    use audiosync_core::ring::Ring;

    use super::*;
    use crate::audio::process::tests::{desktop, entry};

    /// AudioSync's process ID in [`desktop`].
    const OWN_PID: u32 = 400;

    fn app(root_pid: u32) -> SourceId {
        SourceId::App { root_pid }
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

    #[test]
    fn an_app_that_got_the_id_of_audiosyncs_exited_launcher_is_refused() {
        // AudioSync's launcher (300) exited, and a player started later got
        // its ID. Windows may still count AudioSync in the player's tree.
        let table = ProcessTable::new([
            entry(100, 90, Some(10), "explorer.exe"),
            entry(300, 100, Some(50), "player.exe"),
            entry(400, 300, Some(40), "audiosync.exe"),
        ]);
        assert_eq!(table.parent(400), None, "not its parent by creation time");
        assert_eq!(
            decide_activation(&app(300), 400, &table),
            Err(BackendError::ContainsAudioSync)
        );
        assert_eq!(
            decide_activation(&app(100), 400, &table),
            Err(BackendError::ContainsAudioSync),
            "nor the app above it"
        );
        let sources = source_list(&table, &[300], 400);
        assert_eq!(sources.len(), 1, "not offered: {sources:?}");
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
        ]);
        let names: Vec<String> = source_list(&table, &[10, 20, 30], 99)
            .into_iter()
            .map(|source| source.name)
            .collect();
        assert_eq!(names, ["All System Audio", "Alpha", "Mid", "Zebra"]);
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

    fn f32_bytes(samples: &[f32]) -> Vec<u8> {
        samples.iter().flat_map(|s| s.to_le_bytes()).collect()
    }

    /// Reads the newest `frames` frames of the ring.
    fn newest(ring: &Arc<Ring>, frames: usize) -> Vec<f32> {
        let mut reader = Reader::new(Arc::clone(ring), SESSION_RATE);
        let mut out = vec![0.0; frames * 2];
        reader.read(&mut out, frames, frames as i64, 0);
        out
    }

    #[test]
    fn float_stereo_at_the_session_rate_is_written_unchanged() {
        let (ring, writer) = session_ring();
        let mut converter = CaptureConverter::new(CAPTURE_ATTEMPTS[0].format, writer).unwrap();
        let samples: Vec<f32> = (0..3000).map(|i| (i as f32 / 3000.0) - 0.5).collect();
        converter.push(&f32_bytes(&samples), 1500, false);
        assert_eq!(ring.frames_written(), 1500);
        assert_eq!(newest(&ring, 1500), samples);
    }

    #[test]
    fn sixteen_bit_samples_are_scaled_to_float() {
        let (ring, writer) = session_ring();
        let format = StreamFormat {
            sample: SampleType::I16,
            rate: SESSION_RATE,
            channels: 2,
        };
        let mut converter = CaptureConverter::new(format, writer).unwrap();
        let samples: [i16; 4] = [i16::MIN, 16_384, 0, -16_384];
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        converter.push(&bytes, 2, false);
        assert_eq!(newest(&ring, 2), [-1.0, 0.5, 0.0, -0.5]);
    }

    #[test]
    fn a_silent_packet_writes_silence_whatever_its_data() {
        let (ring, writer) = session_ring();
        let mut converter = CaptureConverter::new(CAPTURE_ATTEMPTS[0].format, writer).unwrap();
        converter.push(&f32_bytes(&[0.25; 8]), 4, false);
        converter.push(&f32_bytes(&[0.75; 8]), 4, true);
        assert_eq!(ring.frames_written(), 8);
        let out = newest(&ring, 8);
        assert_eq!(&out[..8], &[0.25; 8]);
        assert_eq!(&out[8..], &[0.0; 8]);
    }

    #[test]
    fn a_mono_capture_feeds_both_channels() {
        let (ring, writer) = session_ring();
        let format = StreamFormat {
            sample: SampleType::F32,
            rate: SESSION_RATE,
            channels: 1,
        };
        let mut converter = CaptureConverter::new(format, writer).unwrap();
        converter.push(&f32_bytes(&[0.1, 0.2, 0.3]), 3, false);
        assert_eq!(newest(&ring, 3), [0.1, 0.1, 0.2, 0.2, 0.3, 0.3]);
    }

    #[test]
    fn a_short_packet_writes_only_its_whole_frames() {
        let (ring, writer) = session_ring();
        let mut converter = CaptureConverter::new(CAPTURE_ATTEMPTS[0].format, writer).unwrap();
        let mut bytes = f32_bytes(&[0.5; 6]);
        bytes.truncate(bytes.len() - 1);
        converter.push(&bytes, 10, false);
        assert_eq!(ring.frames_written(), 2);
    }

    #[test]
    fn a_long_packet_is_converted_in_chunks() {
        let (ring, writer) = session_ring();
        let mut converter = CaptureConverter::new(CAPTURE_ATTEMPTS[0].format, writer).unwrap();
        let frames = CONVERT_CHUNK * 3 + 17;
        let samples: Vec<f32> = (0..frames * 2)
            .map(|i| (i % 1000) as f32 / 1000.0)
            .collect();
        converter.push(&f32_bytes(&samples), frames, false);
        assert_eq!(ring.frames_written(), frames as u64);
        assert_eq!(newest(&ring, frames), samples);
    }

    #[test]
    fn a_44_1_khz_capture_is_resampled_to_the_session_rate() {
        let (ring, writer) = session_ring();
        let format = CAPTURE_ATTEMPTS[3].format;
        assert_eq!(format.rate, 44_100);
        let mut converter = CaptureConverter::new(format, writer).unwrap();
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

    #[test]
    fn a_rate_outside_the_supported_range_is_refused() {
        let (_ring, writer) = session_ring();
        let format = StreamFormat {
            sample: SampleType::F32,
            rate: 4_000,
            channels: 2,
        };
        assert!(matches!(
            CaptureConverter::new(format, writer),
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
            CaptureConverter::new(format, writer),
            Err(BackendError::UnsupportedFormat)
        ));
    }
}
