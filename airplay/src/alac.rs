//! ALAC payloads (P77, P29).
//!
//! P77 lets the encoder emit compressed or uncompressed frames. The sender
//! writes the uncompressed ("escape") form: every frame is a channel-pair
//! element whose escape flag is set, followed by the samples as they are,
//! then the end element. A frame of 352 stereo frames is 1412 bytes, so a
//! packet with its 12-byte header fits a 1500-byte MTU like a PCM one
//! (P76). Lossless compression would save bandwidth, not latency or
//! quality, on a local network; see `NOTES.md` for why no encoder crate was
//! used.
//!
//! P77 leaves the bitstream to Apple's codec. The layout below is the
//! author's own reading of the ALAC frame syntax, not taken from any codec
//! source; the tests decode every payload with an independent decoder set
//! up from the `a=fmtp` line and must get the input back bit for bit.
//!
//! The configuration matches `a=fmtp:96` (P29, [`crate::sdp::ALAC_FMTP`]):
//! frame length 352, 16 bits, 2 channels, 44 100 Hz. Every frame holds
//! exactly the configured frame length, so no frame carries an explicit
//! sample count.

use crate::audio::{FRAMES_PER_PACKET, Frame, SAMPLE_RATE};

/// Bytes of one uncompressed ALAC frame of 352 stereo frames: a 23-bit
/// element header, 352 × 2 × 16 sample bits and the 3-bit end element,
/// padded to a whole byte.
pub const FRAME_BYTES: usize = (23 + FRAMES_PER_PACKET * 2 * 16 + 3).div_ceil(8);

/// Element tag of a channel pair.
const ID_CHANNEL_PAIR: u32 = 1;
/// Element tag that ends a frame.
const ID_END: u32 = 7;

/// The ALAC configuration record (Apple's "magic cookie", 24 bytes,
/// big-endian) with the values of `a=fmtp:96` (P29): frame length,
/// compatible version, bit depth, the tuning values pb, mb and kb, channel
/// count, maximum run, maximum frame size, average bit rate and sample
/// rate.
pub fn configuration() -> [u8; 24] {
    let mut cookie = [0u8; 24];
    cookie[0..4].copy_from_slice(&(FRAMES_PER_PACKET as u32).to_be_bytes());
    cookie[4] = 0; // compatible version
    cookie[5] = 16; // bit depth
    cookie[6] = 40; // pb
    cookie[7] = 10; // mb
    cookie[8] = 14; // kb
    cookie[9] = 2; // channels
    cookie[10..12].copy_from_slice(&255u16.to_be_bytes()); // maximum run
    cookie[12..16].copy_from_slice(&0u32.to_be_bytes()); // maximum frame bytes
    cookie[16..20].copy_from_slice(&0u32.to_be_bytes()); // average bit rate
    cookie[20..24].copy_from_slice(&SAMPLE_RATE.to_be_bytes());
    cookie
}

/// Writes one uncompressed ALAC frame of exactly 352 stereo frames into
/// `out` (P77). Doesn't allocate.
pub fn write_frame(frames: &[Frame; FRAMES_PER_PACKET], out: &mut [u8; FRAME_BYTES]) {
    let mut bits = BitWriter::new(out);
    bits.put(ID_CHANNEL_PAIR, 3);
    bits.put(0, 4); // element instance
    bits.put(0, 12); // unused header bits
    bits.put(0, 1); // no explicit sample count: the configured 352
    bits.put(0, 2); // no shifted low bytes at 16 bits
    bits.put(1, 1); // escape: uncompressed samples follow
    for frame in frames {
        bits.put(u32::from(frame[0] as u16), 16);
        bits.put(u32::from(frame[1] as u16), 16);
    }
    bits.put(ID_END, 3);
    bits.finish();
}

/// Big-endian bit packing into a fixed buffer.
struct BitWriter<'a> {
    out: &'a mut [u8],
    byte: usize,
    /// Bits already used in `out[byte]`.
    used: u32,
}

impl<'a> BitWriter<'a> {
    fn new(out: &'a mut [u8]) -> BitWriter<'a> {
        out.fill(0);
        BitWriter {
            out,
            byte: 0,
            used: 0,
        }
    }

    /// Appends the low `count` bits of `value`, most significant first.
    fn put(&mut self, value: u32, count: u32) {
        let mut remaining = count;
        while remaining > 0 {
            let room = 8 - self.used;
            let take = room.min(remaining);
            let shift = remaining - take;
            let chunk = (value >> shift) & ((1 << take) - 1);
            self.out[self.byte] |= (chunk << (room - take)) as u8;
            self.used += take;
            remaining -= take;
            if self.used == 8 {
                self.byte += 1;
                self.used = 0;
            }
        }
    }

    /// Pads the last byte with zero bits (they are already zero).
    fn finish(self) {}
}
