//! The realtime stream (type 96, UDP) as a measurement reads it: packet loss
//! induced on the data port with a resend request for each drop (P86), and
//! the latency the sender declares in its sync packets (P93). In its own
//! file so a test includes it.

use std::net::{SocketAddr, UdpSocket};
use std::sync::Mutex;

/// Audio frames per second on the realtime path, for the milliseconds.
pub const RATE: f64 = 44_100.0;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Counts {
    pub dropped: u64,
    pub requested: u64,
    pub resent: u64,
}

#[derive(Default)]
struct State {
    percent: f64,
    credit: f64,
    sender: Option<SocketAddr>,
    request_seq: u16,
    counts: Counts,
}

pub struct Loss {
    control: UdpSocket,
    state: Mutex<State>,
}

fn u16_at(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

impl Loss {
    /// `control` sends the resend requests, as the receiver's control port.
    pub fn new(control: UdpSocket) -> Loss {
        Loss {
            control,
            state: Mutex::new(State::default()),
        }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Drops `percent` of the audio packets from now on, spread evenly; 0 is off.
    pub fn set(&self, percent: f64) {
        let mut state = self.state();
        state.percent = percent;
        state.credit = 0.0;
    }

    pub fn counts(&self) -> Counts {
        self.state().counts
    }

    /// A datagram on the control port. As a receiver does (P86), the first
    /// one's source is where resend requests go; a resent packet is counted.
    pub fn on_control(&self, packet: &[u8], from: SocketAddr) {
        let mut state = self.state();
        state.sender.get_or_insert(from);
        if packet.get(1).map(|kind| kind & 0x7f) == Some(0x56) {
            state.counts.resent += 1;
        }
    }

    /// An audio packet on the data port: when it falls in the lost share,
    /// it is counted as dropped and its sequence number is asked for again.
    pub fn on_data(&self, packet: &[u8]) {
        let mut state = self.state();
        if state.percent <= 0.0 {
            return;
        }
        state.credit += state.percent;
        if state.credit < 100.0 {
            return;
        }
        state.credit -= 100.0;
        state.counts.dropped += 1;
        let (Some(sender), Some(seq)) = (state.sender, u16_at(packet, 2)) else {
            return;
        };
        let [r0, r1] = state.request_seq.to_be_bytes();
        let [s0, s1] = seq.to_be_bytes();
        let request = [0x80, 0xd5, r0, r1, s0, s1, 0, 1];
        if self.control.send_to(&request, sender).is_ok() {
            state.request_seq = state.request_seq.wrapping_add(1);
            state.counts.requested += 1;
        }
    }
}

fn ms(frames: u32) -> f64 {
    f64::from(frames) * 1000.0 / RATE
}

/// The latency a sender declares: a sync packet's next-packet RTP time
/// minus the RTP time it says is heard at its NTP time (P93).
#[derive(Default)]
pub struct Declared {
    frames: Option<u32>,
    logged_at: f64,
}

impl Declared {
    /// The line to log for the sync packet `packet` that arrived `seconds`
    /// after start: at a stream start (the first sync, or one flagged first
    /// after RECORD or FLUSH), on a change, and every 60 s.
    pub fn sync(&mut self, packet: &[u8], seconds: f64) -> Option<String> {
        if packet.len() < 20 || packet[1] & 0x7f != 0x54 {
            return None;
        }
        let frames = u32_at(packet, 16)?.wrapping_sub(u32_at(packet, 4)?);
        let start = self.frames.is_none() || packet[0] & 0x10 != 0;
        let line = match self.frames {
            _ if start => format!("start, {frames} samples ({:.1} ms)", ms(frames)),
            Some(old) if old != frames => format!(
                "{old} -> {frames} samples ({:.1} -> {:.1} ms)",
                ms(old),
                ms(frames)
            ),
            _ if seconds - self.logged_at >= 60.0 => {
                format!("{frames} samples ({:.1} ms)", ms(frames))
            }
            _ => return None,
        };
        self.frames = Some(frames);
        self.logged_at = seconds;
        Some(format!("latency: {line}"))
    }
}
