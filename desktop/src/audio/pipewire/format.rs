//! The format the capture asks PipeWire for, and the check of the format
//! it gets: interleaved 32-bit float at the session rate, stereo, or every
//! channel of an input device that plays a pair past 1-2.
//! PipeWire's adapter converts whatever the output plays to it, so the
//! converter never resamples.

use audiosync_core::channels::InputPair;

use crate::audio::SESSION_RATE;
use crate::audio::backend::BackendError;
use crate::audio::capture::{CaptureAttempt, SampleType, StreamFormat};

/// What the capture asks for and accepts.
pub const CAPTURE_FORMAT: StreamFormat = StreamFormat {
    sample: SampleType::F32,
    rate: SESSION_RATE,
    channels: 2,
};

/// How the capture is set up once PipeWire agreed: the adapter converts.
pub const SETUP: CaptureAttempt = CaptureAttempt {
    format: CAPTURE_FORMAT,
    autoconvert: true,
    pair: InputPair::FIRST,
};

/// How an input device with `channels` channels is captured for the pair
/// stored as `saved`: pair 1-2, or a pair the device no longer has, keeps
/// the adapter's stereo mix of the device, as Windows does; any other pair
/// takes every channel unmixed and plays that pair.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn input_setup(saved: u16, channels: u16) -> CaptureAttempt {
    let (pair, _) = InputPair::resolve(saved, channels);
    if pair.index == 0 {
        return SETUP;
    }
    CaptureAttempt {
        format: StreamFormat {
            channels,
            ..CAPTURE_FORMAT
        },
        autoconvert: true,
        pair,
    }
}

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

/// Whether `negotiated` is the `asked` format: anything else is
/// [`BackendError::UnsupportedFormat`], since the converter is built for
/// that format before the stream connects.
pub fn check(negotiated: Negotiated, asked: StreamFormat) -> Result<(), BackendError> {
    let expected = negotiated.raw_audio
        && negotiated.f32_interleaved
        && negotiated.rate == asked.rate
        && negotiated.channels == u32::from(asked.channels);
    if expected {
        Ok(())
    } else {
        Err(BackendError::UnsupportedFormat)
    }
}

/// The frames a buffer of `size` bytes holds at `stride` bytes per frame;
/// a stride of 0 (not set) means `block_align`, the format's own.
pub fn frames_in(size: u32, stride: i32, block_align: usize) -> usize {
    let stride = usize::try_from(stride)
        .ok()
        .filter(|&stride| stride > 0)
        .unwrap_or(block_align);
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

    const AGREED_SIX: Negotiated = Negotiated {
        channels: 6,
        ..AGREED
    };

    #[test]
    fn the_asked_format_is_accepted() {
        assert_eq!(check(AGREED, CAPTURE_FORMAT), Ok(()));
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
            assert_eq!(
                check(other, CAPTURE_FORMAT),
                Err(BackendError::UnsupportedFormat)
            );
        }
    }

    // covers: S14
    #[test]
    fn a_pair_past_1_2_takes_every_channel_unmixed() {
        let setup = input_setup(1, 6);
        assert_eq!(
            setup.format,
            StreamFormat {
                sample: SampleType::F32,
                rate: SESSION_RATE,
                channels: 6,
            }
        );
        assert_eq!(
            setup.pair,
            InputPair {
                index: 1,
                left: 2,
                right: 3,
            }
        );
        assert_eq!(check(AGREED_SIX, setup.format), Ok(()));
        assert_eq!(
            check(AGREED, setup.format),
            Err(BackendError::UnsupportedFormat)
        );
    }

    // covers: S14
    #[test]
    fn pair_1_2_and_a_pair_the_device_lost_keep_the_stereo_mix() {
        for (saved, channels) in [(0, 6), (5, 6), (1, 2)] {
            let setup = input_setup(saved, channels);
            assert_eq!(setup.format, CAPTURE_FORMAT);
            assert_eq!(setup.pair, InputPair::FIRST);
        }
    }

    #[test]
    fn frames_follow_the_stride() {
        assert_eq!(frames_in(8192, 8, 8), 1024);
        assert_eq!(frames_in(8190, 8, 8), 1023);
        assert_eq!(frames_in(8192, 0, 8), 1024);
        assert_eq!(frames_in(8192, -8, 8), 1024);
    }
}
