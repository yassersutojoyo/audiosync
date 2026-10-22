//! The encrypted framing of the AirPlay 2 connections (P67 to P70): each
//! frame is a 2-byte little-endian length N, N bytes of ChaCha20-Poly1305
//! ciphertext and the 16-byte tag, under a key per direction from
//! HKDF-SHA-512 of the session's shared secret, with a 64-bit frame counter
//! per direction in the nonce and the length field as additional data.

use chacha20poly1305::aead::{AeadInOut, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Tag};
use zeroize::Zeroizing;

use crate::homekit::keys::{self, TAG_BYTES};
use crate::pairing::keys::KEY_BYTES;

/// The most plaintext the sender puts in one frame (P67).
pub const MAX_FRAME_PLAINTEXT: usize = 1024;
/// Bytes of the length field.
pub const LENGTH_BYTES: usize = 2;

/// The RTSP connection's keys (P68).
pub const CONTROL_SALT: &[u8] = b"Control-Salt";
pub const CONTROL_WRITE: &[u8] = b"Control-Write-Encryption-Key";
pub const CONTROL_READ: &[u8] = b"Control-Read-Encryption-Key";
/// The event connection's keys (P70).
pub const EVENTS_SALT: &[u8] = b"Events-Salt";
pub const EVENTS_WRITE: &[u8] = b"Events-Write-Encryption-Key";
pub const EVENTS_READ: &[u8] = b"Events-Read-Encryption-Key";

/// A frame whose tag doesn't check: the connection can't be trusted any
/// more (P140).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameError;

/// One framed connection: the sealing key and counter of the sender's
/// direction, the opening key and counter of the receiver's, and the bytes
/// of a frame that hasn't fully arrived.
pub struct Framing {
    seal_key: Zeroizing<[u8; KEY_BYTES]>,
    open_key: Zeroizing<[u8; KEY_BYTES]>,
    sent: u64,
    received: u64,
    pending: Vec<u8>,
    failed: bool,
}

impl std::fmt::Debug for Framing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Framing")
            .field("sent", &self.sent)
            .field("received", &self.received)
            .finish_non_exhaustive()
    }
}

/// The 12-byte nonce of frame `counter`: four zero bytes, then the counter
/// in little-endian (P69).
pub fn frame_nonce(counter: u64) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[4..].copy_from_slice(&counter.to_le_bytes());
    nonce
}

impl Framing {
    fn new(secret: &[u8], salt: &[u8], seal_info: &[u8], open_info: &[u8]) -> Framing {
        Framing {
            seal_key: keys::derive(secret, (salt, seal_info)),
            open_key: keys::derive(secret, (salt, open_info)),
            sent: 0,
            received: 0,
            pending: Vec::new(),
            failed: false,
        }
    }

    /// The RTSP connection: the sender seals with the Write key and opens
    /// with the Read key (P68).
    pub fn control(secret: &[u8]) -> Framing {
        Framing::new(secret, CONTROL_SALT, CONTROL_WRITE, CONTROL_READ)
    }

    /// The event connection, roles swapped: the sender seals with the Read
    /// key and opens with the Write key (P70).
    pub fn events(secret: &[u8]) -> Framing {
        Framing::new(secret, EVENTS_SALT, EVENTS_READ, EVENTS_WRITE)
    }

    /// Frames sealed so far, and frames opened so far.
    pub fn counters(&self) -> (u64, u64) {
        (self.sent, self.received)
    }

    /// The plaintext in frames of at most [`MAX_FRAME_PLAINTEXT`] bytes.
    pub fn seal(&mut self, plaintext: &[u8]) -> Vec<u8> {
        let frames = plaintext.len().div_ceil(MAX_FRAME_PLAINTEXT);
        let mut out = Vec::with_capacity(plaintext.len() + frames * (LENGTH_BYTES + TAG_BYTES));
        let cipher = ChaCha20Poly1305::new((&*self.seal_key).into());
        for piece in plaintext.chunks(MAX_FRAME_PLAINTEXT) {
            // A piece is at most 1024 bytes, so its length fits the field.
            let length = (piece.len() as u16).to_le_bytes();
            let mut sealed = piece.to_vec();
            let tag = cipher
                .encrypt_inout_detached(
                    &frame_nonce(self.sent).into(),
                    &length,
                    sealed.as_mut_slice().into(),
                )
                .map(|tag| tag.to_vec())
                .unwrap_or_else(|_| vec![0; TAG_BYTES]);
            self.sent = self.sent.wrapping_add(1);
            out.extend_from_slice(&length);
            out.extend_from_slice(&sealed);
            out.extend_from_slice(&tag);
        }
        out
    }

    /// Takes bytes as they arrive and gives the plaintext of every frame
    /// they complete, however the frames are split across reads. Frames of
    /// any length up to 65 535 are read (P67). After one bad tag every
    /// later call fails too.
    pub fn open(&mut self, bytes: &[u8]) -> Result<Vec<u8>, FrameError> {
        if self.failed {
            return Err(FrameError);
        }
        self.pending.extend_from_slice(bytes);
        let cipher = ChaCha20Poly1305::new((&*self.open_key).into());
        let mut plaintext = Vec::new();
        let mut at = 0;
        loop {
            let rest = &self.pending[at..];
            if rest.len() < LENGTH_BYTES {
                break;
            }
            let length = [rest[0], rest[1]];
            let size = usize::from(u16::from_le_bytes(length));
            let whole = LENGTH_BYTES + size + TAG_BYTES;
            if rest.len() < whole {
                break;
            }
            let mut opened = rest[LENGTH_BYTES..LENGTH_BYTES + size].to_vec();
            let tag = Tag::try_from(&rest[LENGTH_BYTES + size..whole]);
            let checked = tag.is_ok_and(|tag| {
                cipher
                    .decrypt_inout_detached(
                        &frame_nonce(self.received).into(),
                        &length,
                        opened.as_mut_slice().into(),
                        &tag,
                    )
                    .is_ok()
            });
            if !checked {
                self.failed = true;
                self.pending.clear();
                return Err(FrameError);
            }
            self.received = self.received.wrapping_add(1);
            plaintext.extend_from_slice(&opened);
            at += whole;
        }
        self.pending.drain(..at);
        Ok(plaintext)
    }
}
