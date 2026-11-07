//! Reads the PCM two or more receivers write (16-bit little-endian stereo
//! at 44.1 kHz, from a file or a FIFO each), finds the onset of every click
//! `rx_pair --click` sent, and judges the spec row
//! `hardware_two_speaker_sync`: each stream's onsets must trail the first
//! stream's by its delay difference, within one packet (P102). Every read is
//! timed on one clock in this one process, so the streams compare directly.
//!
//! ```text
//! cargo run --release -p audiosync-airplay --example rx_onset -- \
//!     --pcm <path>[,delay=<ms>] --pcm <path>[,delay=<ms>] [--seconds <n>] [--threshold <n>] \
//!     [--reads <dir>]
//! ```
//!
//! Stops when every stream ends or after `--seconds` (60 by default), then
//! prints one `stream` and one `onset` line per stream and click, one `pair`
//! line per click both streams heard, and a last line `PIPE PASS` or
//! `PIPE FAIL`. Exits 0 on PASS, 1 on FAIL, 2 on bad arguments. Development
//! only.
//!
//! A receiver writing to a pipe isn't paced exactly in real time and the
//! reads come back late by varying amounts, so each onset is timed on the
//! receiver's play timeline, frame f at `anchor + f / 44 100`, anchored on
//! the reads before the first click, not by the read that carried it. A
//! `drift` line per stream pair checks the onsets' frame offset holds within
//! one packet across the clicks. A last `SPREAD` line times each click
//! from the reads in the second before it instead and gives the largest
//! spread of one click across every stream that heard it from
//! `SETTLE_S` after its first read, so a receiver turned on later is judged
//! too; a `SETTLING` line gives the same over every click, the first
//! `SETTLE_S` of each stream included.

#[path = "rx_pair/sync_check.rs"]
mod sync_check;

use std::fs::File;
use std::io::Read;
use std::process::ExitCode;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use sync_check::{
    DEFAULT_THRESHOLD, Detector, MAX_DELAY_MS, RATE, click_spread, local_seconds, verdict,
};

/// An AirPlay 2 test receiver's PTP helper takes seconds to line up after it
/// starts, and in WSL the clock it reads steps back about 0.6 s every 30 s;
/// a receiver that just started lines up from a different state than one
/// already playing (WSL runs, 8 Nov 2026: a joiner was off by 40 ms 1 s
/// after its first read, 8 ms at 5 s, up to 5 ms at 9 to 13 s, then within
/// 0.2 ms).
const SETTLE_S: f64 = 20.0;

struct Stream {
    path: String,
    delay: Duration,
    detector: Arc<Mutex<Detector>>,
    first_read: Arc<Mutex<Option<f64>>>,
    last_read: Arc<Mutex<Option<f64>>>,
}

struct Args {
    streams: Vec<(String, Duration)>,
    seconds: u64,
    threshold: i32,
    reads: Option<String>,
}

fn usage() -> &'static str {
    "usage: rx_onset --pcm <path>[,delay=<ms>] --pcm <path>[,delay=<ms>] [--seconds <n>] [--threshold <n>] [--reads <dir>]"
}

fn parse_pcm(text: &str) -> Result<(String, Duration), String> {
    let Some((path, option)) = text.rsplit_once(",delay=") else {
        return Ok((text.to_owned(), Duration::ZERO));
    };
    let ms: u64 = option
        .parse()
        .map_err(|_| format!("--pcm {text}: delay '{option}' is not whole ms"))?;
    if ms > MAX_DELAY_MS {
        return Err(format!("--pcm {text}: delay above {MAX_DELAY_MS} ms"));
    }
    Ok((path.to_owned(), Duration::from_millis(ms)))
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        streams: Vec::new(),
        seconds: 60,
        threshold: DEFAULT_THRESHOLD,
        reads: None,
    };
    let mut items = std::env::args().skip(1);
    while let Some(item) = items.next() {
        let mut value = |name: &str| items.next().ok_or(format!("{name} needs a value"));
        match item.as_str() {
            "--pcm" => args.streams.push(parse_pcm(&value("--pcm")?)?),
            "--seconds" => {
                let text = value("--seconds")?;
                args.seconds = text
                    .parse()
                    .map_err(|_| format!("--seconds {text}: not a whole number"))?;
            }
            "--reads" => args.reads = Some(value("--reads")?),
            "--threshold" => {
                let text = value("--threshold")?;
                args.threshold = text
                    .parse()
                    .ok()
                    .filter(|level| (1..=i16::MAX as i32).contains(level))
                    .ok_or(format!("--threshold {text}: not 1 to 32767"))?;
            }
            other => return Err(format!("unknown argument '{other}'")),
        }
    }
    if args.streams.len() < 2 {
        return Err("at least two --pcm are needed".to_owned());
    }
    Ok(args)
}

fn read_stream(stream: &Stream, start: Instant, done: mpsc::Sender<()>) {
    let path = stream.path.clone();
    let detector = stream.detector.clone();
    let first_read = stream.first_read.clone();
    let last_read = stream.last_read.clone();
    thread::spawn(move || {
        // Opening a FIFO waits here until its receiver starts writing.
        match File::open(&path) {
            Ok(mut file) => {
                let mut buffer = vec![0u8; 64 * 1024];
                loop {
                    match file.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(count) => {
                            let arrival = start.elapsed().as_secs_f64();
                            first_read.lock().unwrap().get_or_insert(arrival);
                            *last_read.lock().unwrap() = Some(arrival);
                            detector.lock().unwrap().push(&buffer[..count], arrival);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                        Err(error) => {
                            eprintln!("{path}: {error}");
                            break;
                        }
                    }
                }
            }
            Err(error) => eprintln!("{path}: {error}"),
        }
        let _ = done.send(());
    });
}

/// One line per read, frames read so far and arrival minus those frames at
/// RATE, to plot what the timeline was fitted to.
fn write_reads(dir: &str, stream: usize, reads: &[(u64, f64)]) {
    let text: String = reads
        .iter()
        .map(|(frames, offset)| format!("{frames} {offset:.6}\n"))
        .collect();
    let path = format!("{dir}/reads-{stream}.txt");
    if let Err(error) = std::fs::write(&path, text) {
        eprintln!("{path}: {error}");
    }
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}\n{}", usage());
            return ExitCode::from(2);
        }
    };
    let start = Instant::now();
    let streams: Vec<Stream> = args
        .streams
        .into_iter()
        .map(|(path, delay)| Stream {
            path,
            delay,
            detector: Arc::new(Mutex::new(Detector::new(args.threshold))),
            first_read: Arc::default(),
            last_read: Arc::default(),
        })
        .collect();
    let (done, ended) = mpsc::channel();
    for stream in &streams {
        read_stream(stream, start, done.clone());
    }
    let deadline = start + Duration::from_secs(args.seconds);
    for _ in &streams {
        let left = deadline.saturating_duration_since(Instant::now());
        if ended.recv_timeout(left).is_err() {
            break;
        }
    }

    let mut results = Vec::new();
    let mut local = Vec::new();
    let mut settled = Vec::new();
    for stream in &streams {
        let mut detector = stream.detector.lock().unwrap();
        detector.finish();
        if let Some(dir) = &args.reads {
            write_reads(dir, results.len() + 1, detector.reads());
        }
        let first = *stream.first_read.lock().unwrap();
        let last = *stream.last_read.lock().unwrap();
        let rate = match (first, last) {
            (Some(first), Some(last)) if last > first => {
                format!("{:.0}", detector.frames() as f64 / (last - first))
            }
            _ => "none".to_owned(),
        };
        let timeline = detector.timeline().map_or_else(
            || "none".to_owned(),
            |line| format!("anchor_s={:.6} anchor_reads={}", line.anchor, line.reads),
        );
        println!(
            "stream {} delay_ms={} frames={} frames_per_s={rate} nominal={RATE} onsets={} timeline {timeline}",
            stream.path,
            stream.delay.as_millis(),
            detector.frames(),
            detector.onsets().len(),
        );
        for (index, onset) in detector.onsets().iter().enumerate() {
            println!(
                "onset {} {} frame={} t_s={:.6}",
                stream.path,
                index + 1,
                onset.frame,
                onset.seconds
            );
        }
        let times: Vec<f64> = detector
            .onsets()
            .iter()
            .filter_map(|onset| local_seconds(detector.reads(), onset.frame))
            .collect();
        let delay = stream.delay.as_secs_f64();
        settled.push(
            times
                .iter()
                .filter(|&&seconds| first.is_some_and(|first| seconds >= first + SETTLE_S))
                .map(|seconds| seconds - delay)
                .collect(),
        );
        local.push(times.iter().map(|seconds| seconds - delay).collect());
        results.push((
            stream.path.clone(),
            stream.delay,
            detector.onsets().to_vec(),
        ));
    }

    let mut pass = true;
    let (reference_path, reference_delay, reference_onsets) = &results[0];
    for (path, delay, onsets) in &results[1..] {
        let judged = verdict((reference_onsets, *reference_delay), (onsets, *delay));
        for pair in &judged.pairs {
            println!(
                "pair {} {reference_path} t_s={:.6} {path} t_s={:.6} diff_ms={:.2} expected_ms={:.0} error_frames={:.0} frame_diff={} {}",
                pair.index,
                pair.reference,
                pair.other,
                pair.diff_s * 1000.0,
                pair.expected_s * 1000.0,
                pair.error_frames,
                pair.frame_diff,
                if pair.pass { "PASS" } else { "FAIL" },
            );
        }
        println!(
            "drift {reference_path} vs {path}: frame_diff spread {} frames over {} clicks (limit {} frames) {}",
            judged.drift_frames,
            judged.pairs.len(),
            audiosync_airplay::audio::PACKET_FRAMES,
            if judged.drift_pass { "PASS" } else { "FAIL" },
        );
        let first = judged.pairs.first().map_or_else(
            || "no onsets to compare".to_owned(),
            |pair| {
                format!(
                    "first onsets {:.6} and {:.6} s, diff_ms={:.2}, expected_ms={:.0}",
                    pair.reference,
                    pair.other,
                    pair.diff_s * 1000.0,
                    pair.expected_s * 1000.0
                )
            },
        );
        println!(
            "PIPE {} {reference_path} vs {path}: {first}; {} (limit {} frames)",
            if judged.pass { "PASS" } else { "FAIL" },
            judged.reason,
            audiosync_airplay::audio::PACKET_FRAMES,
        );
        pass &= judged.pass;
    }
    let (worst, clicks, _) = click_spread(&local);
    println!(
        "SETTLING {:.3} ms over {clicks} clicks (every click, each timed from the second before it)",
        worst * 1000.0
    );
    let (worst, clicks, everywhere) = click_spread(&settled);
    println!(
        "SPREAD {:.3} ms over {clicks} clicks, {everywhere} heard on every stream (each timed from the second before it, from {SETTLE_S} s after the stream's first read)",
        worst * 1000.0
    );
    if pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
