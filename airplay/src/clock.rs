//! The sender clock (P89).
//!
//! One monotonic clock serves every session: `std::time::Instant`, which is
//! never stepped or slewed by wall-clock changes, counted from an origin the
//! host picks once. It is read as a 64-bit NTP-format timestamp (32-bit
//! seconds, 32-bit fraction, RFC 5905 §6) whose seconds start at 0x83AA7E80,
//! the Unix epoch in NTP seconds, at the origin. The host schedules its
//! local outputs on the same clock by sharing the [`SenderClock`] (P89,
//! P114).

use std::time::{Duration, Instant};

/// NTP seconds at the clock's origin: the Unix epoch in NTP seconds (P89).
pub const NTP_ORIGIN_SECONDS: u64 = 0x83AA_7E80;

/// A 64-bit NTP-format timestamp: seconds in the high 32 bits, the fraction
/// of a second in the low 32 (RFC 5905 §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NtpTime(pub u64);

impl NtpTime {
    pub fn seconds(self) -> u32 {
        (self.0 >> 32) as u32
    }

    pub fn fraction(self) -> u32 {
        self.0 as u32
    }

    pub fn to_be_bytes(self) -> [u8; 8] {
        self.0.to_be_bytes()
    }

    pub fn from_be_bytes(bytes: [u8; 8]) -> NtpTime {
        NtpTime(u64::from_be_bytes(bytes))
    }
}

/// The sender clock: an origin on the monotonic clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SenderClock {
    origin: Instant,
}

impl SenderClock {
    pub fn new(origin: Instant) -> SenderClock {
        SenderClock { origin }
    }

    pub fn origin(&self) -> Instant {
        self.origin
    }

    /// Seconds from the origin to `at`, negative before it.
    pub fn seconds(&self, at: Instant) -> f64 {
        match at.checked_duration_since(self.origin) {
            Some(after) => after.as_secs_f64(),
            None => -self.origin.duration_since(at).as_secs_f64(),
        }
    }

    /// The instant `seconds` after the origin (before it when negative). An
    /// instant the platform can't represent is clamped to the origin.
    pub fn instant(&self, seconds: f64) -> Instant {
        let span = Duration::from_secs_f64(seconds.abs().min(1e12));
        if seconds >= 0.0 {
            self.origin + span
        } else {
            self.origin.checked_sub(span).unwrap_or(self.origin)
        }
    }

    /// `at` as an NTP-format timestamp (P89).
    pub fn ntp(&self, at: Instant) -> NtpTime {
        self.ntp_at(self.seconds(at))
    }

    /// `seconds` after the origin as an NTP-format timestamp, rounded to the
    /// nearest 2^-32 s.
    pub fn ntp_at(&self, seconds: f64) -> NtpTime {
        let origin = i128::from(NTP_ORIGIN_SECONDS) << 32;
        let offset = (seconds * 4_294_967_296.0).round() as i128;
        NtpTime((origin + offset) as u64)
    }

    /// The seconds after the origin that an NTP-format timestamp of this
    /// clock stands for, the inverse of [`SenderClock::ntp_at`].
    pub fn seconds_of(&self, time: NtpTime) -> f64 {
        let origin = i128::from(NTP_ORIGIN_SECONDS) << 32;
        (i128::from(time.0) - origin) as f64 / 4_294_967_296.0
    }
}
