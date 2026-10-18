//! The UDP packets of an AirPlay 1 session: RTP audio (P78), sync (P93),
//! retransmission requests and replies (P86, P87) and the timing exchange
//! (P90, P91), each encoded and parsed without allocation where it can be.

use crate::clock::NtpTime;

/// Bytes of an RTP header without CSRCs (P78).
pub const HEADER_BYTES: usize = 12;
/// The dynamic payload type of the audio (P28, P78).
pub const PAYLOAD_TYPE: u8 = 96;
/// Bytes of a sync packet (P93).
pub const SYNC_BYTES: usize = 20;
/// Bytes of a retransmission request (P86).
pub const RESEND_REQUEST_BYTES: usize = 8;
/// Bytes of a timing request or reply (P90, P91).
pub const TIMING_BYTES: usize = 32;
/// Bytes before the original packet in a retransmission reply (P87).
pub const RESEND_PREFIX_BYTES: usize = 4;

/// Second byte of a sync packet (P93).
pub const TYPE_SYNC: u8 = 0xD4;
/// Second byte of a retransmission request (P86).
pub const TYPE_RESEND_REQUEST: u8 = 0xD5;
/// Second byte of a retransmission reply (P87).
pub const TYPE_RESEND_REPLY: u8 = 0xD6;
/// Second byte of a timing request (P90).
pub const TYPE_TIMING_REQUEST: u8 = 0xD2;
/// Second byte of a timing reply (P91).
pub const TYPE_TIMING_REPLY: u8 = 0xD3;

/// Whether a first byte says RTP version 2, the only version there is.
fn version_2(byte: u8) -> bool {
    byte & 0xC0 == 0x80
}

/// The 12-byte RTP header of an audio packet (P78).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    /// Set on the first packet after RECORD or FLUSH.
    pub marker: bool,
    pub seq: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

impl RtpHeader {
    /// Byte 0 is 0x80 (version 2, no padding, extension or CSRC); byte 1 is
    /// 0x60, or 0xE0 with the marker (P78).
    pub fn encode(&self) -> [u8; HEADER_BYTES] {
        let mut bytes = [0u8; HEADER_BYTES];
        bytes[0] = 0x80;
        bytes[1] = PAYLOAD_TYPE | if self.marker { 0x80 } else { 0 };
        bytes[2..4].copy_from_slice(&self.seq.to_be_bytes());
        bytes[4..8].copy_from_slice(&self.timestamp.to_be_bytes());
        bytes[8..12].copy_from_slice(&self.ssrc.to_be_bytes());
        bytes
    }

    /// The header of an audio packet; `None` unless it is a version 2
    /// header with payload type 96.
    pub fn parse(bytes: &[u8]) -> Option<RtpHeader> {
        if bytes.len() < HEADER_BYTES || !version_2(bytes[0]) || bytes[1] & 0x7F != PAYLOAD_TYPE {
            return None;
        }
        Some(RtpHeader {
            marker: bytes[1] & 0x80 != 0,
            seq: u16::from_be_bytes([bytes[2], bytes[3]]),
            timestamp: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            ssrc: u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
        })
    }
}

/// A sync packet (P93).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncPacket {
    /// The first sync after RECORD or FLUSH: byte 0 is 0x90 (extension bit
    /// set) instead of 0x80.
    pub first: bool,
    /// Counts up from 0 per session (P93).
    pub seq: u16,
    /// The RTP timestamp of the frame due to be heard at `time`.
    pub rtp_heard: u32,
    /// A sender-clock time.
    pub time: NtpTime,
    /// The RTP timestamp of the next audio packet to this receiver.
    pub next_rtp: u32,
}

impl SyncPacket {
    pub fn encode(&self) -> [u8; SYNC_BYTES] {
        let mut bytes = [0u8; SYNC_BYTES];
        bytes[0] = if self.first { 0x90 } else { 0x80 };
        bytes[1] = TYPE_SYNC;
        bytes[2..4].copy_from_slice(&self.seq.to_be_bytes());
        bytes[4..8].copy_from_slice(&self.rtp_heard.to_be_bytes());
        bytes[8..16].copy_from_slice(&self.time.to_be_bytes());
        bytes[16..20].copy_from_slice(&self.next_rtp.to_be_bytes());
        bytes
    }

    pub fn parse(bytes: &[u8]) -> Option<SyncPacket> {
        if bytes.len() != SYNC_BYTES || !version_2(bytes[0]) || bytes[1] != TYPE_SYNC {
            return None;
        }
        let u32_at = |at: usize| {
            u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        let mut time = [0u8; 8];
        time.copy_from_slice(&bytes[8..16]);
        Some(SyncPacket {
            first: bytes[0] & 0x10 != 0,
            seq: u16::from_be_bytes([bytes[2], bytes[3]]),
            rtp_heard: u32_at(4),
            time: NtpTime::from_be_bytes(time),
            next_rtp: u32_at(16),
        })
    }
}

/// A retransmission request from the receiver (P86).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResendRequest {
    /// The request's own sequence field, which the sender doesn't use.
    pub seq: u16,
    /// The first missing sequence number.
    pub first: u16,
    /// How many packets are missing from `first` on.
    pub count: u16,
}

impl ResendRequest {
    /// 8 bytes: 0x80 0xD5, the sequence field, the first missing sequence
    /// number and the count. `None` for anything else, so other packets on
    /// the control socket are ignored.
    pub fn parse(bytes: &[u8]) -> Option<ResendRequest> {
        if bytes.len() < RESEND_REQUEST_BYTES
            || !version_2(bytes[0])
            || bytes[1] != TYPE_RESEND_REQUEST
        {
            return None;
        }
        Some(ResendRequest {
            seq: u16::from_be_bytes([bytes[2], bytes[3]]),
            first: u16::from_be_bytes([bytes[4], bytes[5]]),
            count: u16::from_be_bytes([bytes[6], bytes[7]]),
        })
    }

    /// The request as a receiver sends it (for tests and diagnostics).
    pub fn encode(&self) -> [u8; RESEND_REQUEST_BYTES] {
        let mut bytes = [0u8; RESEND_REQUEST_BYTES];
        bytes[0] = 0x80;
        bytes[1] = TYPE_RESEND_REQUEST;
        bytes[2..4].copy_from_slice(&self.seq.to_be_bytes());
        bytes[4..6].copy_from_slice(&self.first.to_be_bytes());
        bytes[6..8].copy_from_slice(&self.count.to_be_bytes());
        bytes
    }
}

/// The four bytes before the original packet in a retransmission reply
/// (P87): 0x80 0xD6 and the resent packet's sequence number.
pub fn resend_prefix(seq: u16) -> [u8; RESEND_PREFIX_BYTES] {
    let seq = seq.to_be_bytes();
    [0x80, TYPE_RESEND_REPLY, seq[0], seq[1]]
}

/// A timing request (P90) or reply (P91).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimingPacket {
    /// A reply (0xD3) rather than a request (0xD2).
    pub reply: bool,
    pub seq: u16,
    pub origin: NtpTime,
    pub receive: NtpTime,
    pub transmit: NtpTime,
}

impl TimingPacket {
    /// 32 bytes: 0x80, the type, the sequence number, four zero bytes, then
    /// the origin, receive and transmit timestamps.
    pub fn encode(&self) -> [u8; TIMING_BYTES] {
        let mut bytes = [0u8; TIMING_BYTES];
        bytes[0] = 0x80;
        bytes[1] = if self.reply {
            TYPE_TIMING_REPLY
        } else {
            TYPE_TIMING_REQUEST
        };
        bytes[2..4].copy_from_slice(&self.seq.to_be_bytes());
        bytes[8..16].copy_from_slice(&self.origin.to_be_bytes());
        bytes[16..24].copy_from_slice(&self.receive.to_be_bytes());
        bytes[24..32].copy_from_slice(&self.transmit.to_be_bytes());
        bytes
    }

    pub fn parse(bytes: &[u8]) -> Option<TimingPacket> {
        if bytes.len() != TIMING_BYTES || !version_2(bytes[0]) {
            return None;
        }
        let reply = match bytes[1] {
            TYPE_TIMING_REQUEST => false,
            TYPE_TIMING_REPLY => true,
            _ => return None,
        };
        let time_at = |at: usize| {
            let mut time = [0u8; 8];
            time.copy_from_slice(&bytes[at..at + 8]);
            NtpTime::from_be_bytes(time)
        };
        Some(TimingPacket {
            reply,
            seq: u16::from_be_bytes([bytes[2], bytes[3]]),
            origin: time_at(8),
            receive: time_at(16),
            transmit: time_at(24),
        })
    }
}

/// The answer to a timing request (P91): 0x80 0xD3, the request's sequence
/// number, four zero bytes, origin = the request's transmit timestamp as it
/// came, receive = `receive` (the sender clock when the request arrived) and
/// transmit = `transmit` (the sender clock just before sending, which the
/// driver reads right before the socket call). Anything that isn't a timing
/// request gives `None` and is ignored (P92). The reply goes to the
/// request's source address and port, whatever SETUP said (P91).
pub fn timing_reply(request: &[u8], receive: NtpTime, transmit: NtpTime) -> Option<[u8; 32]> {
    let parsed = TimingPacket::parse(request)?;
    if parsed.reply {
        return None;
    }
    Some(
        TimingPacket {
            reply: true,
            seq: parsed.seq,
            origin: parsed.transmit,
            receive,
            transmit,
        }
        .encode(),
    )
}
