//! Length-prefixed framing of `CastMessage`s on the TLS stream (C23, C25).

use prost::Message;

use crate::message::CastMessage;

/// The largest message either side may send, in bytes (C25).
pub const MAX_MESSAGE: usize = 65_536;

/// A frame that breaks the protocol; it closes the channel (C25).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FrameError {
    /// A message, or an inbound length prefix, over [`MAX_MESSAGE`] bytes.
    TooLarge(usize),
    /// Bytes that don't decode as a `CastMessage`.
    Malformed,
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::TooLarge(length) => {
                write!(f, "a {length}-byte message is over the 64 KB limit")
            }
            FrameError::Malformed => f.write_str("a message did not decode as a CastMessage"),
        }
    }
}

impl std::error::Error for FrameError {}

/// The 4-byte big-endian length and the encoded message (C23).
pub fn encode_frame(message: &CastMessage) -> Result<Vec<u8>, FrameError> {
    let length = message.encoded_len();
    if length > MAX_MESSAGE {
        return Err(FrameError::TooLarge(length));
    }
    let mut frame = Vec::with_capacity(4 + length);
    frame.extend_from_slice(&(length as u32).to_be_bytes());
    message
        .encode(&mut frame)
        .map_err(|_| FrameError::Malformed)?;
    Ok(frame)
}

/// Decodes one message body, without its length prefix.
pub fn decode_message(body: &[u8]) -> Result<CastMessage, FrameError> {
    CastMessage::decode(body).map_err(|_| FrameError::Malformed)
}

/// Gathers bytes as they arrive and gives whole messages, however the stream
/// split or joined them (C23).
#[derive(Debug, Default)]
pub struct FrameReader {
    buffer: Vec<u8>,
}

impl FrameReader {
    pub fn new() -> FrameReader {
        FrameReader::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    /// The next whole message, `None` while one is still incomplete.
    pub fn next_message(&mut self) -> Result<Option<CastMessage>, FrameError> {
        let Some(prefix) = self.buffer.first_chunk::<4>() else {
            return Ok(None);
        };
        let length = u32::from_be_bytes(*prefix) as usize;
        // Checked before waiting for the body, so a bad prefix can't make the
        // reader buffer up to 4 GB.
        if length > MAX_MESSAGE {
            return Err(FrameError::TooLarge(length));
        }
        if self.buffer.len() < 4 + length {
            return Ok(None);
        }
        let message = decode_message(&self.buffer[4..4 + length]);
        self.buffer.drain(..4 + length);
        message.map(Some)
    }

    /// Bytes received that are not yet part of a whole message.
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }
}
