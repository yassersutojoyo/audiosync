//! The bad-network switch for test runs (P253): the sender drops and delays
//! its own audio and resend datagrams, so a bad Wi-Fi day's cut-outs can be
//! reproduced at a real receiver. The host decides whether a launch honours
//! [`BAD_NETWORK_ENV`] and hands the parsed values to the sender in
//! [`crate::sender::SenderConfig::bad_network`]; the crate never reads the
//! environment.

use std::collections::VecDeque;
use std::fmt;
use std::time::{Duration, Instant};

/// The environment variable a host reads in a test launch.
pub const BAD_NETWORK_ENV: &str = "AUDIOSYNC_BAD_NETWORK";

/// The highest drop chance, in percent.
pub const MAX_LOSS_PERCENT: f64 = 50.0;

/// The most delay plus spread, in milliseconds.
pub const MAX_DELAY_MS: u32 = 1000;

/// The datagrams a second the delay queue is sized for: sixteen speakers'
/// audio at about 125 packets a second, with room for resends.
const QUEUE_PER_SECOND: usize = 2048;

/// What the switch does to each audio and resend datagram.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BadNetwork {
    /// The chance a datagram is dropped, 0 to 50 %.
    pub loss_percent: f64,
    /// The mean delay of a datagram that is not dropped.
    pub delay_ms: u32,
    /// How far the delay strays either side of `delay_ms`.
    pub spread_ms: u32,
}

impl fmt::Display for BadNetwork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "bad network: loss {}%, delay {} ms, spread {} ms",
            self.loss_percent, self.delay_ms, self.spread_ms
        )
    }
}

/// Why a value was ignored as a whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Malformed {
    /// A part that is not `key=value`, trimmed.
    NotAPair(String),
    UnknownKey(String),
    Repeated(String),
    NotANumber(String),
    OutOfRange(String),
}

impl fmt::Display for Malformed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Malformed::NotAPair(part) => write!(f, "\"{part}\" is not key=value"),
            Malformed::UnknownKey(key) => write!(f, "unknown key \"{key}\""),
            Malformed::Repeated(key) => write!(f, "{key} given twice"),
            Malformed::NotANumber(key) => write!(f, "{key} is not a number"),
            Malformed::OutOfRange(key) => write!(
                f,
                "{key} out of range (loss 0 to {MAX_LOSS_PERCENT}, delay plus spread at most {MAX_DELAY_MS})"
            ),
        }
    }
}

/// The switch for a value of [`BAD_NETWORK_ENV`]: `None` when it is off
/// (empty, or all three at 0).
pub fn parse(value: &str) -> Result<Option<BadNetwork>, Malformed> {
    if value.trim().is_empty() {
        return Ok(None);
    }
    let mut loss = None;
    let mut delay = None;
    let mut spread = None;
    for part in value.split(',') {
        let part = part.trim();
        let Some((key, number)) = part.split_once('=') else {
            return Err(Malformed::NotAPair(part.to_owned()));
        };
        let slot = match key {
            "loss" => {
                if loss.is_some() {
                    return Err(Malformed::Repeated(key.to_owned()));
                }
                let percent = decimal(number).ok_or(Malformed::NotANumber(key.to_owned()))?;
                if percent > MAX_LOSS_PERCENT {
                    return Err(Malformed::OutOfRange(key.to_owned()));
                }
                loss = Some(percent);
                continue;
            }
            "delay" => &mut delay,
            "spread" => &mut spread,
            _ => return Err(Malformed::UnknownKey(key.to_owned())),
        };
        if slot.is_some() {
            return Err(Malformed::Repeated(key.to_owned()));
        }
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Malformed::NotANumber(key.to_owned()));
        }
        match number.parse::<u32>() {
            Ok(ms) if ms <= MAX_DELAY_MS => *slot = Some(ms),
            _ => return Err(Malformed::OutOfRange(key.to_owned())),
        }
    }
    let config = BadNetwork {
        loss_percent: loss.unwrap_or(0.0),
        delay_ms: delay.unwrap_or(0),
        spread_ms: spread.unwrap_or(0),
    };
    if config.delay_ms + config.spread_ms > MAX_DELAY_MS {
        return Err(Malformed::OutOfRange("spread".to_owned()));
    }
    let off = config.loss_percent == 0.0 && config.delay_ms == 0 && config.spread_ms == 0;
    Ok((!off).then_some(config))
}

/// Digits with at most one point, nothing else: no sign, exponent or name.
fn decimal(text: &str) -> Option<f64> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if whole.len() + fraction.len() == 0 || !digits(whole) || !digits(fraction) {
        return None;
    }
    text.parse().ok()
}

/// What a host hands the sender for the variable's value in a test launch,
/// and the one debug log line that says so.
pub fn switch(value: Option<&str>) -> (Option<BadNetwork>, String) {
    match parse(value.unwrap_or_default()) {
        Ok(Some(config)) => (Some(config), config.to_string()),
        Ok(None) => (None, "bad network: off".to_owned()),
        Err(why) => (
            None,
            format!("bad network: off, {BAD_NETWORK_ENV} ignored: {why}"),
        ),
    }
}

/// What the switch did with an offered datagram.
#[derive(Debug, PartialEq, Eq)]
pub enum Fate<T> {
    Dropped,
    /// It goes now.
    Send(T),
    /// It waits in the queue until [`Shaper::pop_due`] gives it back.
    Delayed,
}

struct Held<P, T> {
    due: Instant,
    path: P,
    item: T,
}

/// Drops and delays datagrams as a [`BadNetwork`] says, keeping their order
/// on each path.
pub struct Shaper<P, T> {
    config: BadNetwork,
    rng: u64,
    queue: VecDeque<Held<P, T>>,
}

impl<P: PartialEq, T> Shaper<P, T> {
    /// A shaper whose draws follow from `seed`.
    pub fn new(config: BadNetwork, seed: u64) -> Shaper<P, T> {
        let window_ms = (config.delay_ms + config.spread_ms) as usize;
        Shaper {
            config,
            rng: seed,
            queue: VecDeque::with_capacity(QUEUE_PER_SECOND * window_ms / 1000),
        }
    }

    /// SplitMix64, as a value in [0, 1). Statistics only, never secrets.
    fn unit(&mut self) -> f64 {
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Drops `item` or gives the time it may leave on `path`: never before
    /// a datagram offered ahead of it on the same path.
    pub fn offer(&mut self, path: P, item: T, now: Instant) -> Fate<T> {
        let BadNetwork {
            loss_percent,
            delay_ms,
            spread_ms,
        } = self.config;
        if loss_percent > 0.0 && self.unit() * 100.0 < loss_percent {
            return Fate::Dropped;
        }
        let mut due = now;
        if delay_ms + spread_ms > 0 {
            let low = f64::from(delay_ms) - f64::from(spread_ms);
            let ms = low + self.unit() * 2.0 * f64::from(spread_ms);
            due += Duration::from_secs_f64(ms.max(0.0) / 1000.0);
        }
        let ahead = self.queue.iter().rev().find(|held| held.path == path);
        if let Some(ahead) = ahead {
            due = due.max(ahead.due);
        } else if due <= now {
            return Fate::Send(item);
        }
        self.queue.push_back(Held { due, path, item });
        Fate::Delayed
    }

    /// The next datagram whose time came, in the order offered on its path.
    pub fn pop_due(&mut self, now: Instant) -> Option<T> {
        let at = self.queue.iter().position(|held| held.due <= now)?;
        self.queue.remove(at).map(|held| held.item)
    }

    /// When the next datagram may leave, if one waits.
    pub fn next_due(&self) -> Option<Instant> {
        self.queue.iter().map(|held| held.due).min()
    }
}
