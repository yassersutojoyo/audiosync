//! Reads the PCM two or more receivers write (16-bit little-endian stereo
//! at 44.1 kHz, from a file or a FIFO each), finds the onset of every click
//! `rx_pair --click` sent, and judges the spec row
//! `hardware_two_speaker_sync`: each stream's onsets must trail the first
//! stream's by its delay difference, within one packet (P102). Every read is
//! timed on one clock in this one process, so the streams compare directly.
//!
//! ```text
//! cargo run --release -p audiosync-airplay --example rx_onset -- \
//!     --pcm <path>[,delay=<ms>] --pcm <path>[,delay=<ms>] [--seconds <n>] [--threshold <n>]
//! ```
//!
//! Stops when every stream ends or after `--seconds` (60 by default), then
//! prints one `stream` and one `onset` line per stream and click, one `pair`
//! line per click both streams heard, and a last line `SYNC PASS` or
//! `SYNC FAIL`. Exits 0 on PASS, 1 on FAIL, 2 on bad arguments. Development
//! only.

#[path = "rx_pair/sync_check.rs"]
mod sync_check;

use std::fs::File;
use std::io::Read;
use std::process::ExitCode;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use sync_check::{DEFAULT_THRESHOLD, Detector, MAX_DELAY_MS, RATE, verdict};

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
}

fn usage() -> &'static str {
    "usage: rx_onset --pcm <path>[,delay=<ms>] --pcm <path>[,delay=<ms>] [--seconds <n>] [--threshold <n>]"
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
    for stream in &streams {
        let mut detector = stream.detector.lock().unwrap();
        detector.finish();
        let first = *stream.first_read.lock().unwrap();
        let last = *stream.last_read.lock().unwrap();
        let rate = match (first, last) {
            (Some(first), Some(last)) if last > first => {
                format!("{:.0}", detector.frames() as f64 / (last - first))
            }
            _ => "none".to_owned(),
        };
        println!(
            "stream {} delay_ms={} frames={} frames_per_s={rate} nominal={RATE} onsets={}",
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
                "pair {} {reference_path} t_s={:.6} {path} t_s={:.6} diff_ms={:.2} expected_ms={:.0} error_frames={:.0} {}",
                pair.index,
                pair.reference,
                pair.other,
                pair.diff_s * 1000.0,
                pair.expected_s * 1000.0,
                pair.error_frames,
                if pair.pass { "PASS" } else { "FAIL" },
            );
        }
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
            "SYNC {} {reference_path} vs {path}: {first}; {} (limit {} frames)",
            if judged.pass { "PASS" } else { "FAIL" },
            judged.reason,
            audiosync_airplay::audio::PACKET_FRAMES,
        );
        pass &= judged.pass;
    }
    if pass {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
