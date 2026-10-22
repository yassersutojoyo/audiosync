//! Payload stores (P80, P88, P111).
//!
//! A store keeps the encoded payloads of the last packets of the timeline,
//! keyed by packet number, in memory allocated once. Packets are encoded
//! once and shared by every speaker of the codec (P80); each speaker keeps
//! only its headers. The store covers the resend history of P88 plus the
//! largest per-speaker delay (P111), so a speaker 20 s behind still finds
//! its packets and can resend its last 1024.

use crate::audio::{PACKET_FRAMES, SAMPLE_RATE};
use crate::stream::MAX_DELAY;

/// Packets a session can resend (P88).
pub const HISTORY_PACKETS: usize = 1024;
/// The largest speaker delay, in packets: 20 s (P99, P111).
pub const DELAY_PACKETS: usize =
    (MAX_DELAY.as_millis() as usize * SAMPLE_RATE as usize / 1000).div_ceil(PACKET_FRAMES as usize);
/// Packets for the send margin M of at most 100 ms (P84), and one more.
pub const MARGIN_PACKETS: usize = (SAMPLE_RATE as usize / 10).div_ceil(PACKET_FRAMES as usize) + 1;
/// Packets one store holds (P111).
pub const STORE_PACKETS: usize = HISTORY_PACKETS + DELAY_PACKETS + MARGIN_PACKETS;

const EMPTY: u64 = u64::MAX;

/// Payloads by packet number, all of one size.
#[derive(Debug)]
pub struct PayloadStore {
    payload_bytes: usize,
    numbers: Box<[u64]>,
    silent: Box<[u16]>,
    bytes: Box<[u8]>,
}

impl PayloadStore {
    /// A store for `packets` payloads of `payload_bytes` each, allocated now.
    pub fn new(packets: usize, payload_bytes: usize) -> PayloadStore {
        PayloadStore {
            payload_bytes,
            numbers: vec![EMPTY; packets].into_boxed_slice(),
            silent: vec![0; packets].into_boxed_slice(),
            bytes: vec![0; packets * payload_bytes].into_boxed_slice(),
        }
    }

    /// How many packets it holds.
    pub fn capacity(&self) -> usize {
        self.numbers.len()
    }

    /// Bytes of payload memory.
    pub fn memory_bytes(&self) -> usize {
        self.bytes.len()
    }

    /// Stores packet `number`'s payload, with how many of its frames were
    /// silence the sender inserted, replacing the oldest packet.
    pub fn put(&mut self, number: u64, payload: &[u8], silent: u16) {
        let slot = (number % self.numbers.len() as u64) as usize;
        let start = slot * self.payload_bytes;
        self.bytes[start..start + self.payload_bytes].copy_from_slice(payload);
        self.numbers[slot] = number;
        self.silent[slot] = silent;
    }

    /// Packet `number`'s payload and its silent frames, if still held.
    pub fn get(&self, number: u64) -> Option<(&[u8], u16)> {
        let slot = (number % self.numbers.len() as u64) as usize;
        if self.numbers[slot] != number {
            return None;
        }
        let start = slot * self.payload_bytes;
        Some((
            &self.bytes[start..start + self.payload_bytes],
            self.silent[slot],
        ))
    }
}
