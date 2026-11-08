//! The keyboard's volume keys moving the master volume (spec S429): what a
//! key does to the master, and when the keys are taken from the system.

// Tests never take the keyboard or show a window.
#[cfg(all(windows, not(test)))]
mod hook;
#[cfg(all(windows, not(test)))]
mod popup;

#[cfg(all(windows, not(test)))]
pub use hook::KeyHook;
#[cfg(all(windows, not(test)))]
pub use popup::show as show_level;

/// How far volume up and volume down move the master, as Windows steps its
/// own volume.
pub const STEP: f64 = 2.0;

/// A volume key the hook takes; play/pause and the track keys never are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VolumeKey {
    Up,
    Down,
    Mute,
}

/// The master after `key`, from `master`; `unmute_to` is the level the
/// master's speaker button brings back.
pub fn master_after(key: VolumeKey, master: f64, unmute_to: f64) -> f64 {
    let muted = master < 0.5;
    let level = match key {
        VolumeKey::Up | VolumeKey::Mute if muted => unmute_to,
        VolumeKey::Mute => 0.0,
        VolumeKey::Up => master + STEP,
        VolumeKey::Down => master - STEP,
    }
    .clamp(0.0, 100.0);
    // As the master's slider keeps it: no trace of sound under a muted speaker.
    if level < 0.5 { 0.0 } else { level }
}

/// Whether the keys move the master rather than the system's volume.
pub fn keys_taken(switch_on: bool, streaming: bool) -> bool {
    switch_on && streaming
}

#[cfg(test)]
mod tests {
    use super::*;

    // covers: S429
    #[test]
    fn volume_up_raises_the_master_by_two() {
        assert_eq!(master_after(VolumeKey::Up, 40.0, 50.0), 42.0);
    }

    // covers: S429
    #[test]
    fn volume_down_lowers_the_master_by_two() {
        assert_eq!(master_after(VolumeKey::Down, 40.0, 50.0), 40.0 - STEP);
    }

    // covers: S429
    #[test]
    fn the_master_stays_within_0_and_100_and_a_trace_becomes_0() {
        assert_eq!(master_after(VolumeKey::Up, 99.0, 50.0), 100.0);
        assert_eq!(master_after(VolumeKey::Up, 100.0, 50.0), 100.0);
        assert_eq!(master_after(VolumeKey::Down, 1.0, 50.0), 0.0);
        assert_eq!(master_after(VolumeKey::Down, 2.3, 50.0), 0.0);
    }

    // covers: S429
    #[test]
    fn mute_goes_to_0_and_back_to_the_remembered_level() {
        assert_eq!(master_after(VolumeKey::Mute, 40.0, 40.0), 0.0);
        assert_eq!(master_after(VolumeKey::Mute, 0.0, 40.0), 40.0);
    }

    // covers: S429
    #[test]
    fn volume_up_at_0_brings_back_the_remembered_level_and_down_stays_at_0() {
        assert_eq!(master_after(VolumeKey::Up, 0.0, 64.0), 64.0);
        assert_eq!(master_after(VolumeKey::Down, 0.0, 64.0), 0.0);
    }

    // covers: S429
    #[test]
    fn the_keys_are_taken_only_while_streaming_with_the_switch_on() {
        assert!(keys_taken(true, true));
        assert!(!keys_taken(true, false));
        assert!(!keys_taken(false, true));
        assert!(!keys_taken(false, false));
    }
}
