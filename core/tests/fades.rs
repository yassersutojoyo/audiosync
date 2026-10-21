//! The click-free fades of S182 to S191, the core's checks of the test map's
//! "fades" tables: a renderer's fade-in and fade-out at the device rate, delay
//! changes and held drift corrections between a fade-out and a fade-in, the
//! AirPlay leg's fades, and "no click" as S191 measures it.
//!
//! Unless a test says otherwise, the ring holds a constant 0.5 or a 1 kHz
//! sine of amplitude 0.5 at 48 kHz, the device runs at 48 kHz with 512-frame
//! callbacks, the capture writes 512 frames before each callback, and a fade
//! is 960 frames.

use std::f64::consts::{PI, TAU};
use std::sync::Arc;

use audiosync_core::fade::{Fade, FadeState, InputFade, fade_frames, fade_in_gain, fade_out_gain};
use audiosync_core::ffi::asc_airplay_input_request_fade_out;
use audiosync_core::reader::{Correction, Reader};
use audiosync_core::render::{
    Renderer, RendererControl, STOP_FADE_CAP, fade_tail_frames, faded_by_time,
};
use audiosync_core::ring::{Ring, RingWriter};
use audiosync_core::volume::SharedGain;

const RATE: u32 = 48_000;
const N: usize = 512;
const F: usize = 960;
const A: f32 = 0.5;
const T: i64 = 1680;
/// S191 (a): a frame of a fade is A × g × w within this.
const TOLERANCE: f64 = 1e-6;
const NS: u64 = 1_000_000_000;

/// w of frame `i` of a fade-in of `len` frames, from S182's formula.
fn w_in(i: usize, len: usize) -> f64 {
    if i + 1 >= len {
        return 1.0;
    }
    0.5 - 0.5 * (PI * (i + 1) as f64 / len as f64).cos()
}

/// w of frame `i` of a fade-out of `len` frames, from S182's formula.
fn w_out(i: usize, len: usize) -> f64 {
    if i + 1 >= len {
        return 0.0;
    }
    0.5 + 0.5 * (PI * (i + 1) as f64 / len as f64).cos()
}

/// The largest step of a raised cosine of `len` frames at `amplitude`: S191
/// (b)'s bound, A × g × π / (2F).
fn ramp_step(amplitude: f64, len: usize) -> f64 {
    amplitude * PI / (2.0 * len as f64)
}

/// The largest difference between two consecutive samples.
fn largest_step(samples: &[f32]) -> f64 {
    samples
        .windows(2)
        .map(|pair| (f64::from(pair[1]) - f64::from(pair[0])).abs())
        .fold(0.0, f64::max)
}

/// Asserts that `samples[i]` is `scale × w(i)` within S191's tolerance for
/// the whole fade.
fn assert_fade(samples: &[f32], scale: f64, len: usize, w: fn(usize, usize) -> f64, what: &str) {
    assert!(
        samples.len() >= len,
        "{what}: only {} frames",
        samples.len()
    );
    for (i, &sample) in samples[..len].iter().enumerate() {
        let expected = scale * w(i, len);
        assert!(
            (f64::from(sample) - expected).abs() <= TOLERANCE,
            "{what}: frame {i} is {sample}, the fade says {expected}"
        );
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Signal {
    Constant,
    Sine,
}

fn sample(signal: Signal, index: u64) -> f32 {
    match signal {
        Signal::Constant => A,
        Signal::Sine => {
            (f64::from(A) * (TAU * 1000.0 * index as f64 / f64::from(RATE)).sin()) as f32
        }
    }
}

/// The sine's own largest step between frames, 2A·sin(π·f / R).
fn sine_step() -> f64 {
    2.0 * f64::from(A) * (PI * 1000.0 / f64::from(RATE)).sin()
}

/// A ring, its writer, and one renderer reading it.
struct Rig {
    writer: RingWriter,
    written: u64,
    signal: Signal,
    control: RendererControl,
    gain: Arc<SharedGain>,
    renderer: Renderer,
    channels: usize,
    /// The first channel of every frame rendered so far.
    played: Vec<f32>,
}

impl Rig {
    fn new(delay: i64, signal: Signal) -> Rig {
        Rig::build(delay, signal, RATE, 2, 1.0)
    }

    fn build(delay: i64, signal: Signal, device_rate: u32, channels: usize, gain: f32) -> Rig {
        let ring = Ring::for_rate(RATE).unwrap();
        let writer = ring.writer().unwrap();
        let reader = Reader::new(ring, RATE);
        let control = RendererControl::new(&reader);
        control.set_delay_frames(delay);
        let gain = Arc::new(SharedGain::new(gain));
        let renderer = Renderer::new(
            reader,
            &control,
            Arc::clone(&gain),
            RATE,
            device_rate,
            channels,
        )
        .unwrap();
        Rig {
            writer,
            written: 0,
            signal,
            control,
            gain,
            renderer,
            channels,
            played: Vec::new(),
        }
    }

    /// The capture writes the next `frames` frames of the signal.
    fn write(&mut self, frames: usize) {
        let samples: Vec<f32> = (0..frames as u64)
            .flat_map(|k| {
                let value = sample(self.signal, self.written + k);
                [value, value]
            })
            .collect();
        self.writer.write(&samples, frames);
        self.written += frames as u64;
    }

    /// The capture writes `frames` frames of `value`.
    fn write_value(&mut self, value: f32, frames: usize) {
        self.writer.write(&vec![value; frames * 2], frames);
        self.written += frames as u64;
    }

    /// One callback of `frames` device frames; returns its first channel.
    fn render(&mut self, frames: usize) -> Vec<f32> {
        let mut out = vec![9.0f32; frames * self.channels];
        self.renderer.render(&mut out);
        let left: Vec<f32> = out
            .chunks_exact(self.channels)
            .map(|frame| frame[0])
            .collect();
        self.played.extend_from_slice(&left);
        left
    }

    /// One period: the capture writes N frames, then the device renders N.
    fn tick(&mut self) -> Vec<f32> {
        self.write(N);
        self.render(N)
    }

    fn ticks(&mut self, count: usize) {
        for _ in 0..count {
            self.tick();
        }
    }

    /// Ticks until the first audio has faded in.
    fn play_until_open(&mut self) {
        for _ in 0..400 {
            self.tick();
            if self.renderer.fade_state() == FadeState::Open {
                return;
            }
        }
        panic!("the output never opened");
    }

    /// The first frame rendered at or after `from` that isn't zero.
    fn first_sound_from(&self, from: usize) -> usize {
        from + self.played[from..]
            .iter()
            .position(|&sample| sample != 0.0)
            .expect("the output plays again")
    }
}

// covers: S184, S191
#[test]
fn the_first_audio_an_output_plays_fades_in() {
    let mut rig = Rig::new(1000, Signal::Constant);
    rig.ticks(8);
    let start = rig.first_sound_from(0);
    assert_fade(&rig.played[start..], f64::from(A), F, w_in, "fade-in");
    assert_eq!(rig.played[start + F - 1], A, "w is 1 at the last frame");
    assert!(rig.played[start + F..].iter().all(|&sample| sample == A));
    assert!(
        largest_step(&rig.played) <= ramp_step(f64::from(A), F) + TOLERANCE,
        "no hard edge from silence to the source's level"
    );
}

// covers: S184, S185, S191
#[test]
fn an_output_asked_to_stop_fades_out_and_stays_closed_until_it_stops() {
    let mut rig = Rig::new(1000, Signal::Constant);
    rig.play_until_open();
    rig.ticks(3);
    let from = rig.played.len();
    rig.control.request_fade_out();
    assert!(!rig.control.fade_rendered());
    rig.ticks(2);
    assert_fade(&rig.played[from..], f64::from(A), F, w_out, "fade-out");
    assert_eq!(rig.played[from + F - 1], 0.0, "w is 0 at the last frame");
    assert!(rig.control.fade_rendered());
    // The capture goes on and the reader keeps copying; a delay change
    // doesn't open it again either.
    rig.control.set_delay_frames(3000);
    rig.ticks(40);
    assert!(
        rig.played[from + F - 1..]
            .iter()
            .all(|&sample| sample == 0.0)
    );
    assert_eq!(rig.renderer.fade_state(), FadeState::Closed);
    assert_eq!(
        rig.renderer.frames_after_fade(),
        Some((2 * N - F + 40 * N) as u64),
        "the frames rendered after the fade's last one"
    );
    assert!(largest_step(&rig.played) <= ramp_step(f64::from(A), F) + TOLERANCE);
}

// covers: S185, S191
#[test]
fn with_a_device_that_plays_a_buffer_and_a_safety_offset_late_no_faded_frame_is_cut() {
    let buffer_ns = N as u64 * NS / u64::from(RATE);
    let safety_ns = 1_500_000;
    // The device's and its stream's reported latency, which the host adds.
    let latency_ns = 2_000_000;
    let frame_ns = |offset: usize| offset as u64 * NS / u64::from(RATE);
    let mut rig = Rig::new(1000, Signal::Constant);
    // Each callback k runs at k × buffer, and its first frame is output one
    // buffer plus the safety offset later; a frame is heard once its output
    // time plus the latency has passed, and only if the IO still runs then.
    let mut rendered: Vec<(u64, f32)> = Vec::new();
    let mut callbacks = 0u64;
    let (mut requested, mut rendered_at, mut stopped) = (None, None, None);
    for ms in 0..3_000u64 {
        let now = ms * 1_000_000;
        while callbacks * buffer_ns <= now {
            let at = callbacks * buffer_ns;
            if callbacks == 30 {
                rig.control.request_fade_out();
                requested = Some(at);
            }
            let output = at + buffer_ns + safety_ns;
            rig.write(N);
            let mut out = vec![0.0f32; N * 2];
            rig.renderer.render_timed(&mut out, output);
            for (offset, frame) in out.chunks_exact(2).enumerate() {
                rendered.push((output + frame_ns(offset), frame[0]));
            }
            if rendered_at.is_none() && rig.control.fade_rendered() {
                rendered_at = Some(at);
            }
            callbacks += 1;
        }
        // The control thread: the IO stops once the output counts as faded.
        if requested.is_some() && rig.control.faded_by_time(latency_ns, now) {
            stopped = Some(now);
            break;
        }
    }
    let (requested, rendered_at, stopped) =
        (requested.unwrap(), rendered_at.unwrap(), stopped.unwrap());
    let cut = |stop: u64| {
        rendered
            .iter()
            .filter(|&&(output, sample)| sample != 0.0 && output + latency_ns > stop)
            .count()
    };
    assert_eq!(cut(stopped), 0, "no faded frame is cut");
    assert!(
        cut(rendered_at) > 0,
        "stopping once the fade was rendered would cut its end"
    );
    let bound = buffer_ns + frame_ns(F) + buffer_ns + safety_ns + latency_ns + 1_000_000;
    assert!(stopped - requested <= bound, "{} ns", stopped - requested);
    assert!(u128::from(stopped - requested) <= STOP_FADE_CAP.as_nanos());
}

// covers: S185, S190
#[test]
fn the_published_output_time_is_that_of_the_fades_last_frame() {
    let step = N as u64 * NS / u64::from(RATE);
    let callback = |rig: &mut Rig, time: u64| {
        rig.write(N);
        let mut out = vec![0.0f32; N * 2];
        rig.renderer.render_timed(&mut out, time);
    };
    let mut rig = Rig::new(1000, Signal::Constant);
    let mut time = 7_000_000_000u64;
    for _ in 0..10 {
        callback(&mut rig, time);
        time += step;
    }
    rig.control.request_fade_out();
    // The fade's frames 0 to 511.
    callback(&mut rig, time);
    assert_eq!(rig.control.fade_end_time_ns(), None);
    assert!(!rig.control.faded_by_time(0, u64::MAX));
    time += step;
    // Frames 512 to 959: the last one at offset 447.
    callback(&mut rig, time);
    let end = time + 447 * NS / u64::from(RATE);
    assert_eq!(rig.control.fade_end_time_ns(), Some(end));
    assert!(!rig.control.faded_by_time(3_000_000, end + 2_999_999));
    assert!(rig.control.faded_by_time(3_000_000, end + 3_000_000));
    assert!(faded_by_time(end, 3_000_000, end + 3_000_000));
    assert!(!rig.control.is_faded(), "only the host marks it faded");

    // An output with nothing audible yet has no fade to wait for: the time
    // is its next callback's first frame.
    let mut quiet = Rig::new(48_000, Signal::Constant);
    callback(&mut quiet, 5_000);
    quiet.control.request_fade_out();
    callback(&mut quiet, 9_000);
    assert_eq!(quiet.control.fade_end_time_ns(), Some(9_000));
}

// covers: S182
#[test]
fn fades_follow_the_raised_cosine_over_20_ms_at_the_streams_rate() {
    assert_eq!(fade_frames(48_000), 960);
    assert_eq!(fade_frames(44_100), 882);
    assert_eq!(fade_frames(96_000), 1920);
    assert_eq!(fade_frames(22_050), 441);
    for rate in [48_000, 44_100, 96_000] {
        let len = fade_frames(rate);
        let mut fade = Fade::new(rate);
        assert_eq!(fade.state(), FadeState::Closed);
        assert_eq!(fade.next_gain(), 0.0);
        fade.fade_in();
        assert_eq!(fade.state(), FadeState::FadingIn);
        for i in 0..len {
            let w = fade.next_gain();
            assert!(
                (f64::from(w) - w_in(i, len)).abs() <= TOLERANCE,
                "{rate}: in {i}"
            );
            assert!((f64::from(fade_in_gain(i, len)) - w_in(i, len)).abs() <= TOLERANCE);
        }
        assert_eq!(fade_in_gain(len - 1, len), 1.0);
        assert_eq!(fade.state(), FadeState::Open);
        assert_eq!(fade.next_gain(), 1.0);
        fade.fade_out();
        for i in 0..len {
            let w = fade.next_gain();
            assert!(
                (f64::from(w) - w_out(i, len)).abs() <= TOLERANCE,
                "{rate}: out {i}"
            );
            assert!((f64::from(fade_out_gain(i, len)) - w_out(i, len)).abs() <= TOLERANCE);
        }
        assert_eq!(fade_out_gain(len - 1, len), 0.0);
        assert_eq!(fade.state(), FadeState::Closed);
    }

    // Open frames pass unchanged; closed ones are exact zeros, whatever the
    // input; fading ones never turn NaN or infinity into anything but 0.
    let input = [0.3f32, f32::NAN, f32::INFINITY, -0.7];
    let mut open = input;
    Fade::opened(RATE).apply(&mut open, 2);
    assert_eq!(open.map(f32::to_bits), input.map(f32::to_bits));
    let mut closed = input;
    Fade::new(RATE).apply(&mut closed, 2);
    assert_eq!(closed, [0.0; 4]);
    let mut fading = Fade::new(RATE);
    fading.fade_in();
    let mut frames = [f32::NAN, f32::NEG_INFINITY, 0.5, 0.5];
    fading.apply(&mut frames, 2);
    assert_eq!(frames[..2], [0.0, 0.0]);
    assert!(frames[2] > 0.0 && frames[2] < 1e-5);

    // A renderer counts F in device frames.
    let rig = Rig::build(1000, Signal::Constant, 44_100, 2, 1.0);
    assert_eq!(rig.renderer.fade_len(), 882);
}

// covers: S182
#[test]
fn a_fade_reversed_midway_never_jumps() {
    for rate in [48_000, 44_100] {
        let len = fade_frames(rate);
        let bound = ramp_step(1.0, len) + TOLERANCE;
        for turn in [1, 2, 300, len / 2, len - 2, len - 1] {
            // A fade-in turned into a fade-out after `turn` frames.
            let mut fade = Fade::new(rate);
            fade.fade_in();
            let mut levels = vec![0.0f32];
            for _ in 0..turn {
                levels.push(fade.next_gain());
            }
            assert_eq!(fade.level(), *levels.last().unwrap());
            fade.fade_out();
            let heading = fade;
            fade.fade_out();
            assert_eq!(
                fade, heading,
                "a request for where it is heading changes nothing"
            );
            let back = levels.len();
            while !fade.is_closed() {
                levels.push(fade.next_gain());
            }
            assert_eq!(*levels.last().unwrap(), 0.0);
            assert_eq!(
                levels.len() - back,
                turn,
                "{rate}: back down in {turn} frames"
            );
            assert!(
                largest_step(&levels) <= bound,
                "{rate}: in, turned at {turn}"
            );

            // A fade-out turned into a fade-in.
            let mut fade = Fade::opened(rate);
            fade.fade_out();
            let mut levels = vec![1.0f32];
            for _ in 0..turn {
                levels.push(fade.next_gain());
            }
            fade.fade_in();
            let back = levels.len();
            while !fade.is_open() {
                levels.push(fade.next_gain());
            }
            assert_eq!(*levels.last().unwrap(), 1.0);
            assert_eq!(
                levels.len() - back,
                turn,
                "{rate}: back up in {turn} frames"
            );
            assert!(
                largest_step(&levels) <= bound,
                "{rate}: out, turned at {turn}"
            );
        }
    }
    let mut open = Fade::opened(RATE);
    open.fade_in();
    assert_eq!(open.state(), FadeState::Open);
    let mut closed = Fade::new(RATE);
    closed.fade_out();
    assert_eq!(closed.state(), FadeState::Closed);
    let mut rising = Fade::new(RATE);
    rising.fade_in();
    rising.next_gain();
    let before = rising;
    rising.fade_in();
    assert_eq!(rising, before);
}

// covers: S183
#[test]
fn the_fade_multiplies_after_the_gain_and_a_closed_output_is_exact_zeros_even_for_nan() {
    let mut rig = Rig::build(1000, Signal::Constant, RATE, 2, 0.5);
    rig.ticks(2);
    let start = rig.first_sound_from(0);
    // A gain change during the fade-in ramps over one callback (S64); the
    // fade carries on unchanged.
    rig.gain.set(0.25);
    let first = rig.played.len();
    rig.tick();
    let step = (0.25f32 - 0.5) / N as f32;
    for k in 0..N {
        let gain = if k + 1 == N {
            0.25
        } else {
            0.5 + step * (k + 1) as f32
        };
        let expected = f64::from(A) * f64::from(gain) * w_in(first + k - start, F);
        let got = f64::from(rig.played[first + k]);
        assert!(
            (got - expected).abs() <= TOLERANCE,
            "frame {k}: {got} for {expected}"
        );
    }
    rig.ticks(3);
    assert!(
        rig.played[first + N..]
            .iter()
            .all(|&sample| sample == A * 0.25)
    );
    assert_eq!(rig.gain.get(), 0.25, "the fade never changes the gain");

    // Closed after a stop: exact zeros, whatever the ring holds.
    rig.control.request_fade_out();
    rig.ticks(2);
    let closed = rig.played.len();
    for _ in 0..6 {
        rig.write_value(f32::NAN, N / 2);
        rig.write_value(f32::INFINITY, N / 2);
        rig.render(N);
    }
    assert!(rig.played[closed..].iter().all(|&sample| sample == 0.0));

    // NaN and ±infinity during a fade-in come out as 0, never as NaN.
    let mut odd = Rig::new(1000, Signal::Constant);
    for round in 0..8 {
        odd.write(N - 2);
        odd.write_value(
            if round % 2 == 0 {
                f32::NAN
            } else {
                f32::NEG_INFINITY
            },
            2,
        );
        odd.render(N);
    }
    assert!(
        odd.played
            .iter()
            .all(|sample| sample.is_finite() && sample.abs() <= A)
    );

    // At gain 0 a start stays exact zeros.
    let mut muted = Rig::build(1000, Signal::Constant, RATE, 2, 0.0);
    muted.ticks(8);
    assert!(muted.played.iter().all(|&sample| sample == 0.0));
}

// covers: S184
#[test]
fn a_read_reports_its_leading_silence_and_its_copied_frames() {
    let ring = Ring::for_rate(RATE).unwrap();
    let mut writer = ring.writer().unwrap();
    let mut reader = Reader::new(Arc::clone(&ring), RATE);
    reader.control().set_delay_frames(4096);
    let mut dst = vec![0.0f32; N * 2];
    let block = vec![A; N * 2];
    let split = |report: &audiosync_core::reader::ReadReport| {
        (report.leading_silence(), report.copied, report.starved)
    };
    // Waiting to prime: the whole block is silence at its start.
    writer.write(&block, N);
    let report = reader.read_target(&mut dst, N);
    assert_eq!(split(&report), (N, 0, 0));
    // The prime: all copied.
    for _ in 0..12 {
        writer.write(&block, N);
    }
    let report = reader.read_target(&mut dst, N);
    assert_eq!(report.correction, Correction::Prime);
    assert_eq!(split(&report), (0, N, 0));
    // 300 frames more, then a writer stall: the reader runs ahead until it
    // steps back by 1748 frames; those it already played are silence at the
    // start of the next blocks (three whole ones, then 212 frames), then it
    // copies the 300 it hadn't played.
    writer.write(&block[..300 * 2], 300);
    let mut reads = Vec::new();
    for _ in 0..7 {
        let report = reader.read_target(&mut dst, N);
        reads.push((report.correction, split(&report), report.masked));
    }
    assert_eq!(reads[3], (Correction::StepBack, (N, 0, 0), N));
    assert_eq!(reads[6], (Correction::None, (212, 300, 0), 212));
    assert!(dst[..212 * 2].iter().all(|&sample| sample == 0.0));
    assert!(dst[212 * 2..].iter().all(|&sample| sample == A));
    // A reader at a short delay that reaches W: copies, then silence; a dry
    // read is silence after no copy at all.
    let mut near = Reader::new(Arc::clone(&ring), RATE);
    near.control().set_delay_frames(0);
    let report = near.read_target(&mut dst, N);
    assert_eq!(report.correction, Correction::Prime);
    assert_eq!(split(&report), (0, N, 0));
    writer.write(&block[..200 * 2], 200);
    let report = near.read_target(&mut dst, N);
    assert_eq!(split(&report), (0, 200, N - 200));
    let report = near.read_target(&mut dst, N);
    assert_eq!(split(&report), (0, 0, N));
}

// covers: S184
#[test]
fn audio_after_a_masked_range_or_a_dry_read_fades_in() {
    // A dry read: the writer stalls until the reader has nothing left.
    let mut rig = Rig::new(0, Signal::Constant);
    rig.play_until_open();
    rig.ticks(2);
    for _ in 0..3 {
        rig.render(N);
    }
    let resume = rig.played.len();
    assert!(rig.played[resume - N..].iter().all(|&sample| sample == 0.0));
    rig.ticks(4);
    assert_fade(
        &rig.played[resume..],
        f64::from(A),
        F,
        w_in,
        "after a dry read",
    );
    assert!(rig.played[resume + F..].iter().all(|&sample| sample == A));

    // A masked range: a reader that ran ahead during a stall steps back after
    // its fade-out, and the audio after the masked silence fades in.
    let mut rig = Rig::new(4096, Signal::Constant);
    rig.play_until_open();
    rig.ticks(4);
    let stall = rig.played.len();
    for _ in 0..40 {
        rig.render(N);
        if rig.control.counters().drift_silences > 0 {
            break;
        }
    }
    assert_eq!(rig.control.counters().drift_silences, 1, "stepped back");
    rig.ticks(12);
    let silent = stall
        + rig.played[stall..]
            .iter()
            .position(|&sample| sample == 0.0)
            .unwrap();
    let start = rig.first_sound_from(silent);
    assert!(start - silent > T as usize, "the masked range is silent");
    assert_fade(
        &rig.played[start..],
        f64::from(A),
        F,
        w_in,
        "after the masked range",
    );
    assert!(largest_step(&rig.played) <= ramp_step(f64::from(A), F) + TOLERANCE);
}

// covers: S184
#[test]
fn with_conversion_the_fade_in_starts_at_the_first_copied_frame_or_a_few_frames_before() {
    let device = 44_100;
    let len = fade_frames(device);
    let mut rig = Rig::build(1000, Signal::Constant, device, 2, 1.0);
    // The capture writes what the device consumes: 512 device frames are
    // about 557.3 capture frames.
    let mut owed = 0.0f64;
    for _ in 0..16 {
        owed += N as f64 * f64::from(RATE) / f64::from(device);
        let frames = owed.floor();
        owed -= frames;
        rig.write(frames as usize);
        rig.render(N);
    }
    let first = rig.first_sound_from(0);
    // The converter delays the audio by a few frames, so the fade may start
    // that much before it: never above a fade-in that started EARLY frames
    // before the first sound, and at full level once that fade is over.
    const EARLY: usize = 32;
    for m in 0..len {
        let envelope = 1.1 * f64::from(A) * w_in(m + EARLY, len);
        let got = f64::from(rig.played[first + m]).abs();
        assert!(
            got <= envelope + TOLERANCE,
            "frame {m}: {got} above {envelope}"
        );
    }
    for &sample in &rig.played[first + len + EARLY..] {
        assert!((sample - A).abs() < 1e-3, "{sample}");
    }
}

// covers: S187, S191
#[test]
fn a_delay_change_waits_for_the_fade_out_and_lands_exactly() {
    let mut rig = Rig::new(4800, Signal::Constant);
    rig.play_until_open();
    rig.ticks(4);
    assert_eq!(rig.control.lag_frames(), 4800 - N as i64);
    let resyncs = rig.control.counters().resyncs;
    let from = rig.played.len();
    rig.control.set_delay_frames(2400);
    assert_eq!(rig.control.delay_frames(), 2400);
    rig.tick();
    // The reader plays at the old delay while the renderer fades out.
    assert_eq!(rig.control.reader().delay_frames(), 4800);
    assert_eq!(rig.control.lag_frames(), 4800 - N as i64);
    rig.tick();
    // The fade-out's last frame is behind: the reader has the new delay.
    assert_eq!(rig.control.reader().delay_frames(), 2400);
    assert_fade(&rig.played[from..], f64::from(A), F, w_out, "fade-out");
    assert!(
        rig.played[from + F - 1..from + 2 * N]
            .iter()
            .all(|&sample| sample == 0.0)
    );
    rig.tick();
    assert_eq!(rig.control.counters().resyncs, resyncs + 1);
    assert_eq!(
        rig.control.lag_frames(),
        2400 - N as i64,
        "lands exactly on the new delay (S46)"
    );
    rig.ticks(4);
    assert_fade(
        &rig.played[from + 2 * N..],
        f64::from(A),
        F,
        w_in,
        "fade-in",
    );
    assert_eq!(rig.control.lag_frames(), 2400 - N as i64);
    assert!(largest_step(&rig.played) <= ramp_step(f64::from(A), F) + TOLERANCE);
}

// covers: S187
#[test]
fn a_delay_increase_after_the_fade_out_still_inserts_exactly_the_difference() {
    let mut rig = Rig::new(2400, Signal::Constant);
    rig.play_until_open();
    rig.ticks(4);
    let from = rig.played.len();
    rig.control.set_delay_frames(4800);
    rig.ticks(14);
    // The fade-out's last frame is from + F - 1; the rest of its block is
    // closed; then the resync read masks exactly the difference.
    let start = rig.first_sound_from(from + F - 1);
    assert_eq!(start - (from + 2 * N), 2400, "exactly Δ frames of silence");
    assert_fade(&rig.played[start..], f64::from(A), F, w_in, "fade-in");
    assert_eq!(rig.control.lag_frames(), 4800 - N as i64);
    let counters = rig.control.counters();
    assert_eq!(counters.resyncs, 1);
    assert_eq!((counters.drift_skips, counters.drift_silences), (0, 0));
}

// covers: S187
#[test]
fn a_newer_delay_change_during_the_fade_out_replaces_the_waiting_one() {
    let mut rig = Rig::new(4800, Signal::Constant);
    rig.play_until_open();
    rig.ticks(4);
    let from = rig.played.len();
    rig.control.set_delay_frames(2400);
    rig.tick();
    rig.control.set_delay_frames(3600);
    rig.tick();
    // The fade-out wasn't restarted: it ended F frames after it began.
    assert_fade(&rig.played[from..], f64::from(A), F, w_out, "fade-out");
    assert_eq!(rig.control.reader().delay_frames(), 3600);
    rig.ticks(12);
    assert_eq!(
        rig.control.counters().resyncs,
        1,
        "one resync, at the newest delay"
    );
    assert_eq!(rig.control.lag_frames(), 3600 - N as i64);
}

// covers: S184, S187, S191
#[test]
fn after_a_fade_out_for_a_delay_change_frames_stay_zero_until_the_resync_read() {
    // At a delay of one block the reader is at W after every callback.
    let mut rig = Rig::new(0, Signal::Constant);
    rig.play_until_open();
    rig.ticks(4);
    let from = rig.played.len();
    rig.control.set_delay_frames(1536);
    rig.ticks(2);
    assert_fade(&rig.played[from..], f64::from(A), F, w_out, "fade-out");
    // The writer stalls: the reader is dry, so the change waits for it, and
    // every frame stays zero meanwhile.
    for _ in 0..4 {
        rig.render(N);
    }
    assert_eq!(rig.control.counters().resyncs, 0, "waits for the writer");
    rig.ticks(8);
    assert_eq!(rig.control.counters().resyncs, 1);
    let start = rig.first_sound_from(from + F - 1);
    assert!(start >= from + 6 * N, "zero through the stall");
    assert!(
        rig.played[from + F - 1..start]
            .iter()
            .all(|&sample| sample == 0.0)
    );
    assert_fade(&rig.played[start..], f64::from(A), F, w_in, "fade-in");
    assert!(largest_step(&rig.played) <= ramp_step(f64::from(A), F) + TOLERANCE);
}

// covers: S188
#[test]
fn a_held_drift_correction_is_made_after_the_fade_out_and_counted_then() {
    let mut rig = Rig::new(4096, Signal::Constant);
    rig.play_until_open();
    rig.ticks(4);
    let from = rig.played.len();
    // A burst: the capture delivers 2400 frames more than usual, so the
    // reader is 2400 behind, beyond T.
    rig.write(2400);
    rig.tick();
    assert_eq!(rig.control.counters().drift_skips, 0, "held");
    assert_eq!(rig.control.lag_frames(), 4096 + 2400 - N as i64);
    rig.tick();
    assert_eq!(
        rig.control.counters().drift_skips,
        0,
        "held while it fades out"
    );
    rig.tick();
    let counters = rig.control.counters();
    assert_eq!(
        counters.drift_skips, 1,
        "made by the first read after the fade-out"
    );
    assert_eq!(counters.drift_skip_frames, 2400);
    assert_eq!(rig.control.lag_frames(), 4096 - N as i64);
    rig.ticks(3);
    assert_fade(&rig.played[from..], f64::from(A), F, w_out, "fade-out");
    assert_fade(
        &rig.played[from + 2 * N..],
        f64::from(A),
        F,
        w_in,
        "fade-in",
    );
}

// covers: S184, S188, S191
#[test]
fn after_a_fade_out_for_a_held_correction_frames_stay_zero_until_the_correcting_read() {
    let mut rig = Rig::new(4096, Signal::Constant);
    rig.play_until_open();
    rig.ticks(4);
    let from = rig.played.len();
    rig.write(2400);
    rig.ticks(6);
    // The frames copied after the fade-out's last one, in its block, stay
    // zero; the correcting read's first frame starts the fade-in.
    assert!(
        rig.played[from + F - 1..from + 2 * N]
            .iter()
            .all(|&sample| sample == 0.0)
    );
    assert_eq!(rig.first_sound_from(from + F - 1), from + 2 * N);
    assert!(largest_step(&rig.played) <= ramp_step(f64::from(A), F) + TOLERANCE);

    // A step back in a stall: zero from the fade-out through the masked
    // silence, until the first frame copied after it.
    let mut rig = Rig::new(4096, Signal::Constant);
    rig.play_until_open();
    rig.ticks(4);
    let stall = rig.played.len();
    for _ in 0..6 {
        rig.render(N);
    }
    rig.ticks(12);
    let fade_end = stall
        + rig.played[stall..]
            .iter()
            .position(|&sample| sample == 0.0)
            .unwrap();
    let start = rig.first_sound_from(fade_end);
    assert!(
        rig.played[fade_end..start]
            .iter()
            .all(|&sample| sample == 0.0)
    );
    assert_fade(&rig.played[start..], f64::from(A), F, w_in, "fade-in");
    assert!(largest_step(&rig.played) <= ramp_step(f64::from(A), F) + TOLERANCE);
}

// covers: S188
#[test]
fn a_held_correction_stays_within_t_plus_2n_plus_the_fade() {
    let bound = T + 2 * N as i64 + F as i64;
    assert_eq!(bound, 3664);
    // A writer that runs fast, then slow: the error drifts out of the dead
    // band each way and is corrected after each fade-out.
    let delay = 4096i64;
    let mut rig = Rig::new(delay, Signal::Constant);
    rig.play_until_open();
    let mut worst = 0i64;
    for round in 0..4_000usize {
        let frames = if (round / 1_000) % 2 == 0 {
            N + 7
        } else {
            N - 7
        };
        let error = rig.control.lag_frames() + frames as i64 - delay;
        worst = worst.max(error.abs());
        rig.write(frames);
        rig.render(N);
    }
    let counters = rig.control.counters();
    assert!(counters.drift_skips > 0 && counters.drift_silences > 0);
    assert!(worst <= bound, "worst deviation {worst}");

    // A writer stall at a hold-back above T + 2N + F: the reader steps back
    // after each fade-out and never reaches W.
    let delay = 4000;
    let mut rig = Rig::new(delay, Signal::Constant);
    rig.play_until_open();
    rig.ticks(4);
    let before = rig.control.counters();
    for _ in 0..300 {
        rig.render(N);
    }
    let after = rig.control.counters();
    assert!(
        after.drift_silences >= before.drift_silences + 2,
        "steps back"
    );
    assert_eq!(
        after.starved_renders, before.starved_renders,
        "never reaches W"
    );
}

// covers: S188
#[test]
fn holding_corrections_leaves_prime_resync_and_dry_reads_as_they_were() {
    let ring = Ring::for_rate(RATE).unwrap();
    let mut writer = ring.writer().unwrap();
    let mut held = Reader::new(Arc::clone(&ring), RATE);
    let mut free = Reader::new(Arc::clone(&ring), RATE);
    for reader in [&held, &free] {
        reader.control().set_delay_frames(2048);
    }
    let block: Vec<f32> = (0..N * 2).map(|i| i as f32 / 4096.0).collect();
    let (mut a, mut b) = (vec![0.0f32; N * 2], vec![0.0f32; N * 2]);
    // Waits, the prime, steady reads, a resync each way, a stall into dry
    // reads, and the writer back.
    let mut script: Vec<(usize, Option<i64>)> = vec![(N, None); 8];
    script.push((N, Some(4096)));
    script.extend([(N, None); 12]);
    script.push((N, Some(1024)));
    script.extend([(N, None); 4]);
    script.extend([(0, None); 6]);
    script.extend([(N, None); 6]);
    for (step, (write, delay)) in script.into_iter().enumerate() {
        writer.write(&block[..write * 2], write);
        if let Some(delay) = delay {
            held.control().set_delay_frames(delay);
            free.control().set_delay_frames(delay);
        }
        let x = held.read_target_held(&mut a, N);
        let y = free.read_target(&mut b, N);
        assert_eq!(x, y, "step {step}");
        assert_eq!(a, b, "step {step}");
        assert_eq!(x.due, Correction::None);
    }
    assert_eq!(held.control().counters(), free.control().counters());
    let counters = held.control().counters();
    assert!(counters.primes == 1 && counters.resyncs == 2 && counters.starved_renders > 0);

    // A burst puts both 1976 frames behind: the free reader skips forward at
    // once, the held one only reports it due and counts nothing.
    writer.write(&vec![0.25f32; 3000 * 2], 3000);
    let x = held.read_target_held(&mut a, N);
    let y = free.read_target(&mut b, N);
    assert_eq!(
        (x.correction, x.due),
        (Correction::None, Correction::SkipForward)
    );
    assert_eq!(
        (y.correction, y.due),
        (Correction::SkipForward, Correction::None)
    );
    assert_eq!(held.control().counters().drift_skips, 0);
    // Its next unheld read makes it, against that read's W.
    writer.write(&block, N);
    let x = held.read_target(&mut a, N);
    assert_eq!(x.correction, Correction::SkipForward);
    assert_eq!(x.start, x.written - x.delay);
    assert_eq!(held.control().counters().drift_skips, 1);
}

// covers: S189
#[test]
fn the_native_input_fades_in_after_each_rate_set_and_out_on_a_stop_request() {
    let block = vec![A; 480 * 2];
    let run = |input: &mut InputFade, blocks: usize| -> Vec<f32> {
        let mut out = Vec::new();
        for _ in 0..blocks {
            input.process(&block, |piece| {
                out.extend(piece.chunks_exact(2).map(|frame| frame[0]));
            });
        }
        out
    };
    // Before its first rate set the input is closed.
    let mut input = InputFade::new(RATE, 256);
    assert!(run(&mut input, 2).iter().all(|&sample| sample == 0.0));
    input.restart(RATE);
    let out = run(&mut input, 4);
    assert_eq!(out.len(), 4 * 480, "every frame is handed on");
    assert_fade(&out, f64::from(A), F, w_in, "after the rate set");
    assert!(out[F..].iter().all(|&sample| sample == A));
    input.fade_out();
    let out = run(&mut input, 4);
    assert_fade(&out, f64::from(A), F, w_out, "after the stop request");
    assert!(out[F - 1..].iter().all(|&sample| sample == 0.0));
    assert_eq!(input.state(), FadeState::Closed);
    // The next capture's rate set fades in again, over F at its rate.
    input.restart(44_100);
    let out = run(&mut input, 4);
    assert_fade(&out, f64::from(A), 882, w_in, "at 44.1 kHz");
    assert!(out[882..].iter().all(|&sample| sample == A));
    assert!(
        block.iter().all(|&sample| sample == A),
        "the block is left alone"
    );
    // The C interface's request is harmless on a null handle.
    // SAFETY: a null handle is allowed.
    unsafe { asc_airplay_input_request_fade_out(std::ptr::null_mut()) };
}

// covers: S191
#[test]
fn no_step_across_a_start_a_stop_or_a_delay_change_exceeds_the_ramps_slope() {
    for (signal, gain) in [
        (Signal::Constant, 1.0f32),
        (Signal::Constant, 0.8),
        (Signal::Sine, 1.0),
        (Signal::Sine, 0.8),
    ] {
        let mut rig = Rig::build(2400, signal, RATE, 2, gain);
        // A start (S184), a delay increase and a decrease (S187), a held
        // skip forward (S188), and a stop (S185).
        rig.ticks(12);
        rig.control.set_delay_frames(4800);
        rig.ticks(20);
        rig.control.set_delay_frames(1200);
        rig.ticks(20);
        rig.write(2400);
        rig.ticks(12);
        let counters = rig.control.counters();
        assert_eq!((counters.resyncs, counters.drift_skips), (2, 1));
        rig.control.request_fade_out();
        rig.ticks(8);
        let amplitude = f64::from(A) * f64::from(gain);
        let own = match signal {
            Signal::Constant => 0.0,
            Signal::Sine => sine_step() * f64::from(gain),
        };
        let bound = own + ramp_step(amplitude, F) + TOLERANCE;
        let step = largest_step(&rig.played);
        assert!(
            step <= bound,
            "{signal:?} at {gain}: a step of {step}, over {bound}"
        );
        // (c): exact zeros after the stop's fade-out.
        let tail = rig.played.len() - 6 * N;
        assert!(rig.played[tail..].iter().all(|&sample| sample == 0.0));
        assert!(rig.control.fade_rendered());
    }
}

/// The Windows rule of S185, the endpoint's clock or padding against the frames
/// written after the fade, belongs to the renderer too.
// covers: S185
#[test]
fn an_endpoint_counts_as_faded_once_its_clock_passed_the_fade_or_by_padding_and_latency() {
    // The clock 30 ms past a 10 ms buffer at 48 kHz: that is the tail.
    assert_eq!(fade_tail_frames(480, Some(1_920), 0, RATE), 1_920);
    // Never less than the buffer, never more than the buffer plus 100 ms.
    assert_eq!(fade_tail_frames(480, Some(100), 0, RATE), 480);
    assert_eq!(fade_tail_frames(480, Some(48_000), 0, RATE), 480 + 4_800);
    assert_eq!(
        fade_tail_frames(441, Some(u64::MAX), 0, 44_100),
        441 + 4_410
    );
    // No clock: 480 frames still queued, 10 ms of stream latency.
    assert_eq!(fade_tail_frames(480, None, 480, RATE), 960);
    assert_eq!(fade_tail_frames(u64::MAX, None, u64::MAX, RATE), u64::MAX);
}
