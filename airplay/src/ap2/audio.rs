//! Sealed realtime audio packets of the AirPlay 2 path (P155, P156): the
//! RTP header in the clear, the payload sealed with ChaCha20-Poly1305 under
//! the stream key `shk`, the 16-byte tag, then the 8-byte nonce counter.

use std::fmt;

use chacha20poly1305::aead::{AeadInOut, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Tag};
use zeroize::Zeroizing;

use crate::ap2::bodies::STREAM_KEY_BYTES;
use crate::rtp::HEADER_BYTES;

/// Bytes of the tag (RFC 8439).
pub const TAG_BYTES: usize = 16;
/// Bytes of the nonce trailer.
pub const NONCE_TRAILER_BYTES: usize = 8;
/// How much longer a sealed packet is than its payload: header, tag and
/// trailer (P155).
pub const SEALED_OVERHEAD: usize = HEADER_BYTES + TAG_BYTES + NONCE_TRAILER_BYTES;

/// The stream key `shk` of one stream SETUP (P156). Its bytes never appear
/// in `Debug` output.
#[derive(Clone, PartialEq, Eq)]
pub struct StreamKey(Zeroizing<[u8; STREAM_KEY_BYTES]>);

impl StreamKey {
    pub fn new(bytes: [u8; STREAM_KEY_BYTES]) -> StreamKey {
        StreamKey(Zeroizing::new(bytes))
    }

    pub(crate) fn bytes(&self) -> &[u8; STREAM_KEY_BYTES] {
        &self.0
    }
}

impl fmt::Debug for StreamKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StreamKey(..)")
    }
}

/// The 12-byte nonce of packet `counter`: four zero bytes, then the counter
/// in little-endian (P155).
pub fn packet_nonce(counter: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&counter.to_le_bytes());
    nonce
}

/// Seals the packets of one stream key, counting from 0 (P155, P156).
pub struct PacketSealer {
    cipher: ChaCha20Poly1305,
    next: u64,
}

impl fmt::Debug for PacketSealer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PacketSealer")
            .field("next", &self.next)
            .finish_non_exhaustive()
    }
}

impl PacketSealer {
    pub fn new(key: &StreamKey) -> PacketSealer {
        PacketSealer {
            cipher: ChaCha20Poly1305::new(key.bytes().into()),
            next: 0,
        }
    }

    /// The counter the next packet gets.
    pub fn next_counter(&self) -> u64 {
        self.next
    }

    /// Writes the sealed packet of `header` and `payload` into `out`, which
    /// must be exactly `payload.len() + SEALED_OVERHEAD` bytes long. The
    /// additional data is header bytes 4 to 11.
    pub fn seal_into(&mut self, header: &[u8; HEADER_BYTES], payload: &[u8], out: &mut [u8]) {
        let end = HEADER_BYTES + payload.len();
        out[..HEADER_BYTES].copy_from_slice(header);
        out[HEADER_BYTES..end].copy_from_slice(payload);
        let counter = self.next;
        // A 64-bit counter never wraps within a stream, so no nonce repeats.
        self.next = self.next.wrapping_add(1);
        let tag = self
            .cipher
            .encrypt_inout_detached(
                &packet_nonce(counter).into(),
                &header[4..HEADER_BYTES],
                (&mut out[HEADER_BYTES..end]).into(),
            )
            .unwrap_or_else(|_| Tag::default());
        out[end..end + TAG_BYTES].copy_from_slice(&tag);
        out[end + TAG_BYTES..end + TAG_BYTES + NONCE_TRAILER_BYTES]
            .copy_from_slice(&counter.to_le_bytes());
    }
}
