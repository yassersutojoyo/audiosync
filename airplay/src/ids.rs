//! Random identifiers of one session (P5, P22, P23, P32, P37).
//!
//! Every identifier the sender puts on the wire is random and fresh for each
//! session; none is derived from a hardware address, serial number or user
//! name (P5). The state machines never draw random numbers themselves: the
//! driver makes a [`SessionIds`] from an [`Entropy`] source and hands it in,
//! which keeps them deterministic under test.

use std::fmt;

/// A source of random bytes.
pub trait Entropy {
    /// Fills `buf` with random bytes.
    fn fill(&mut self, buf: &mut [u8]) -> Result<(), EntropyUnavailable>;
}

/// The operating system's random generator, through the `getrandom` crate.
#[derive(Debug, Default, Clone, Copy)]
pub struct OsEntropy;

impl Entropy for OsEntropy {
    fn fill(&mut self, buf: &mut [u8]) -> Result<(), EntropyUnavailable> {
        getrandom::fill(buf).map_err(|_| EntropyUnavailable)
    }
}

/// The random source failed; no session can start without fresh identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntropyUnavailable;

impl fmt::Display for EntropyUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("the random number source is unavailable")
    }
}

impl std::error::Error for EntropyUnavailable {}

/// The identifiers of one session, all random (P5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionIds {
    /// The session ID of the request URI and the SDP `o=` line, written in
    /// decimal (P23, P28).
    pub session_id: u32,
    /// `Client-Instance` and `DACP-ID`, written as 16 uppercase hex digits
    /// (P22).
    pub client_instance: u64,
    /// `Active-Remote`, written in decimal (P22).
    pub active_remote: u32,
    /// Sequence number of the first audio packet, announced in RECORD (P32).
    pub first_seq: u16,
    /// RTP timestamp of the first audio packet, announced in RECORD (P32).
    pub first_rtptime: u32,
    /// Client nonce for digest authentication with `qop=auth` (P37).
    pub cnonce: u64,
}

impl SessionIds {
    /// Draws a fresh set of identifiers.
    pub fn generate(entropy: &mut impl Entropy) -> Result<SessionIds, EntropyUnavailable> {
        let mut bytes = [0u8; 30];
        entropy.fill(&mut bytes)?;
        let u32_at = |at: usize| {
            u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        let u64_at = |at: usize| (u64::from(u32_at(at)) << 32) | u64::from(u32_at(at + 4));
        Ok(SessionIds {
            session_id: u32_at(0),
            client_instance: u64_at(4),
            active_remote: u32_at(12),
            first_seq: u16::from_be_bytes([bytes[16], bytes[17]]),
            first_rtptime: u32_at(18),
            cnonce: u64_at(22),
        })
    }

    /// `Client-Instance` and `DACP-ID`: the same 16 uppercase hex digits (P22).
    pub fn client_instance_hex(&self) -> String {
        format!("{:016X}", self.client_instance)
    }

    /// The digest client nonce as it goes on the wire (P37).
    pub fn cnonce_hex(&self) -> String {
        format!("{:016x}", self.cnonce)
    }
}
