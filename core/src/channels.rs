//! Channel mapping at the capture edge of the pipeline: an input device's
//! channels to interleaved stereo (S14), and the channel pairs a device with
//! more than two input channels offers. The output edge (stereo to a device's
//! channels, S63) belongs to the renderer's output stage.
//!
//! The conversions work a whole interleaved buffer at a time and are
//! real-time safe: no allocation, lock or system call (S126).

#![forbid(unsafe_code)]

/// Converts an interleaved input block with `channels` channels to interleaved
/// stereo (S14): the first two channels become L and R, a one-channel input is
/// copied to both, and channels beyond the second are ignored.
///
/// Converts as many whole frames as both buffers hold and returns that count.
/// Nothing is written when `channels` is 0.
pub fn input_to_stereo(input: &[f32], channels: usize, stereo: &mut [f32]) -> usize {
    pair_to_stereo(input, channels, InputPair::FIRST, stereo)
}

/// Converts an interleaved input block with `channels` channels to interleaved
/// stereo, `pair`'s channels as L and R (S14). A right channel the input
/// doesn't have plays the left one on both sides; a left channel it doesn't
/// have converts nothing.
///
/// Converts as many whole frames as both buffers hold and returns that count.
pub fn pair_to_stereo(
    input: &[f32],
    channels: usize,
    pair: InputPair,
    stereo: &mut [f32],
) -> usize {
    let left = usize::from(pair.left);
    if channels == 0 || left >= channels {
        return 0;
    }
    let right = usize::from(pair.right);
    let right = if right < channels { right } else { left };
    let mut frames = 0;
    for (source, target) in input.chunks_exact(channels).zip(stereo.chunks_exact_mut(2)) {
        target[0] = source[left];
        target[1] = source[right];
        frames += 1;
    }
    frames
}

/// A pair of an input device's channels that plays as L and R (S14). A device
/// with more than two channels offers them two by two, an odd last channel
/// alone; the choice is kept per device and is 1-2 unless changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputPair {
    /// 0 for channels 1 and 2, 1 for 3 and 4, and so on: the stored form.
    pub index: u16,
    /// The channels, numbered from 0; the same one twice for a lone channel.
    pub left: u16,
    pub right: u16,
}

impl InputPair {
    /// Channels 1 and 2. A one-channel device has no second channel, so its
    /// only one plays on both sides.
    pub const FIRST: InputPair = InputPair {
        index: 0,
        left: 0,
        right: 1,
    };

    /// Whether the pair is a single channel.
    pub fn is_mono(self) -> bool {
        self.left == self.right
    }

    /// The pairs to choose from on a device with `channels` input channels;
    /// none up to two channels, where there is nothing to choose.
    pub fn offered(channels: u16) -> Vec<InputPair> {
        if channels <= 2 {
            return Vec::new();
        }
        (0..channels.div_ceil(2))
            .map(|index| InputPair {
                index,
                left: 2 * index,
                right: (2 * index + 1).min(channels - 1),
            })
            .collect()
    }

    /// The pair stored as `saved` on a device with `channels` input
    /// channels, and whether it fell back: a pair the device no longer has
    /// gives 1-2, so the caller can log it.
    pub fn resolve(saved: u16, channels: u16) -> (InputPair, bool) {
        if saved == 0 {
            return (InputPair::FIRST, false);
        }
        match InputPair::offered(channels).get(usize::from(saved)) {
            Some(pair) => (*pair, false),
            None => (InputPair::FIRST, true),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mono_or_stereo_device_offers_no_pairs() {
        assert!(InputPair::offered(0).is_empty());
        assert!(InputPair::offered(1).is_empty());
        assert!(InputPair::offered(2).is_empty());
    }

    #[test]
    fn channels_are_offered_two_by_two_and_an_odd_last_one_alone() {
        let four: Vec<(u16, u16, u16)> = InputPair::offered(4)
            .iter()
            .map(|pair| (pair.index, pair.left, pair.right))
            .collect();
        assert_eq!(four, [(0, 0, 1), (1, 2, 3)]);
        let five = InputPair::offered(5);
        assert_eq!(five.len(), 3);
        assert_eq!((five[2].left, five[2].right), (4, 4));
        assert!(five[2].is_mono() && !five[1].is_mono());
    }

    #[test]
    fn a_saved_pair_is_read_while_the_device_has_it_and_falls_back_to_1_2_after() {
        assert_eq!(InputPair::resolve(0, 8), (InputPair::FIRST, false));
        assert_eq!(InputPair::resolve(0, 1), (InputPair::FIRST, false));
        assert_eq!(InputPair::resolve(1, 4), (InputPair::offered(4)[1], false));
        assert_eq!(InputPair::resolve(2, 5), (InputPair::offered(5)[2], false));
        assert_eq!(InputPair::resolve(2, 4), (InputPair::FIRST, true));
        assert_eq!(InputPair::resolve(1, 2), (InputPair::FIRST, true));
        assert_eq!(InputPair::resolve(u16::MAX, 8), (InputPair::FIRST, true));
    }

    #[test]
    fn a_pair_plays_its_channels_as_left_and_right() {
        let input = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        let mut stereo = [0.0; 4];
        let pair = InputPair::offered(4)[1];
        assert_eq!(pair_to_stereo(&input, 4, pair, &mut stereo), 2);
        assert_eq!(stereo, [3.0, 4.0, 7.0, 8.0]);
    }

    #[test]
    fn a_lone_channel_plays_on_both_sides() {
        let input = [1.0, 2.0, 3.0, 4.0, 5.0, 10.0, 20.0, 30.0, 40.0, 50.0];
        let mut stereo = [0.0; 4];
        let pair = InputPair::offered(5)[2];
        assert_eq!(pair_to_stereo(&input, 5, pair, &mut stereo), 2);
        assert_eq!(stereo, [5.0, 5.0, 50.0, 50.0]);
    }

    #[test]
    fn the_first_pair_keeps_the_mono_and_stereo_behavior() {
        let mut stereo = [0.0; 6];
        assert_eq!(input_to_stereo(&[1.0, 2.0, 3.0], 1, &mut stereo), 3);
        assert_eq!(stereo, [1.0, 1.0, 2.0, 2.0, 3.0, 3.0]);
        let mut stereo = [0.0; 2];
        assert_eq!(input_to_stereo(&[1.0, 2.0, 3.0, 4.0], 4, &mut stereo), 1);
        assert_eq!(stereo, [1.0, 2.0]);
        assert_eq!(input_to_stereo(&[1.0], 0, &mut stereo), 0);
    }

    #[test]
    fn a_pair_beyond_the_input_converts_nothing() {
        let mut stereo = [9.0; 2];
        let pair = InputPair::offered(4)[1];
        assert_eq!(pair_to_stereo(&[1.0, 2.0], 2, pair, &mut stereo), 0);
        assert_eq!(stereo, [9.0, 9.0]);
    }
}
