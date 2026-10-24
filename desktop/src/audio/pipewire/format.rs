//! The one format the capture asks PipeWire for, and the check of the
//! format it gets: interleaved 32-bit float, stereo, at the session rate.
//! PipeWire's adapter converts whatever the output plays to it, so the
//! converter never resamples.

#[cfg(target_os = "linux")]
use audiosync_core::channels::InputPair;

use crate::audio::SESSION_RATE;
use crate::audio::backend::BackendError;
#[cfg(target_os = "linux")]
use crate::audio::capture::CaptureAttempt;
use crate::audio::capture::{SampleType, StreamFormat};

/// What the capture asks for and accepts.
pub const CAPTURE_FORMAT: StreamFormat = StreamFormat {
    sample: SampleType::F32,
    rate: SESSION_RATE,
    channels: 2,
};

/// How the capture is set up once PipeWire agreed: the adapter converts.
#[cfg(target_os = "linux")]
pub const SETUP: CaptureAttempt = CaptureAttempt {
    format: CAPTURE_FORMAT,
    autoconvert: true,
    pair: InputPair::FIRST,
};

/// The format PipeWire settled on, as it reported it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Negotiated {
    /// Raw audio, rather than an encoded or video format.
    pub raw_audio: bool,
    /// The sample format is interleaved little-endian 32-bit float.
    pub f32_interleaved: bool,
    pub rate: u32,
    pub channels: u32,
}

/// Whether `negotiated` is [`CAPTURE_FORMAT`]: anything else is
/// [`BackendError::UnsupportedFormat`], since the converter is built for
/// that format before the stream connects.
pub fn check(negotiated: Negotiated) -> Result<(), BackendError> {
    let expected = negotiated.raw_audio
        && negotiated.f32_interleaved
        && negotiated.rate == CAPTURE_FORMAT.rate
        && negotiated.channels == u32::from(CAPTURE_FORMAT.channels);
    if expected {
        Ok(())
    } else {
        Err(BackendError::UnsupportedFormat)
    }
}

/// The frames a buffer of `size` bytes holds at `stride` bytes per frame;
/// a stride of 0 (not set) means the format's own.
pub fn frames_in(size: u32, stride: i32) -> usize {
    let stride = usize::try_from(stride)
        .ok()
        .filter(|&stride| stride > 0)
        .unwrap_or_else(|| CAPTURE_FORMAT.block_align());
    size as usize / stride
}

#[cfg(test)]
mod tests {
    use super::*;

    const AGREED: Negotiated = Negotiated {
        raw_audio: true,
        f32_interleaved: true,
        rate: 48_000,
        channels: 2,
    };

    #[test]
    fn the_asked_format_is_accepted() {
        assert_eq!(check(AGREED), Ok(()));
    }

    #[test]
    fn any_other_format_is_unsupported() {
        let others = [
            Negotiated {
                raw_audio: false,
                ..AGREED
            },
            Negotiated {
                f32_interleaved: false,
                ..AGREED
            },
            Negotiated {
                rate: 44_100,
                ..AGREED
            },
            Negotiated {
                channels: 6,
                ..AGREED
            },
        ];
        for other in others {
            assert_eq!(check(other), Err(BackendError::UnsupportedFormat));
        }
    }

    #[test]
    fn frames_follow_the_stride() {
        assert_eq!(frames_in(8192, 8), 1024);
        assert_eq!(frames_in(8190, 8), 1023);
        assert_eq!(frames_in(8192, 0), 1024);
        assert_eq!(frames_in(8192, -8), 1024);
    }
}
