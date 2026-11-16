//! A local output's own Windows volume, remembered per endpoint ID (S472):
//! the level it last had while it played through AudioSync is set again
//! before its first audio, unless the level was changed in Windows since,
//! and the level it was found at goes back after its last. The controller
//! does the endpoint part.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Two levels closer than this are the same level.
const SAME: f64 = 0.001;

/// What the settings file keeps, levels 0 to 1 by endpoint ID.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LocalVolumes {
    /// The level each output last had while it played through AudioSync.
    pub remembered: BTreeMap<String, f64>,
    /// The level each output had when it started playing, to put back when
    /// it stops; stored, so one a crash left behind goes back at the next launch.
    pub found: BTreeMap<String, f64>,
    /// The level each output was left at when it last stopped playing; a
    /// different level at its next switch-on was set in Windows since, and wins.
    pub left: BTreeMap<String, f64>,
}

impl LocalVolumes {
    pub fn is_empty(&self) -> bool {
        self.remembered.is_empty() && self.found.is_empty() && self.left.is_empty()
    }

    /// Drops levels that aren't numbers and keeps the rest within 0 to 1.
    #[must_use]
    pub fn clamped(self) -> LocalVolumes {
        let clamp = |levels: BTreeMap<String, f64>| {
            levels
                .into_iter()
                .filter(|(_, level)| level.is_finite())
                .map(|(id, level)| (id, level.clamp(0.0, 1.0)))
                .collect()
        };
        LocalVolumes {
            remembered: clamp(self.remembered),
            found: clamp(self.found),
            left: clamp(self.left),
        }
    }
}

/// The stored levels and the outputs playing through AudioSync now.
#[derive(Debug, Default)]
pub struct LocalVolumeMemory {
    pub stored: LocalVolumes,
    playing: BTreeSet<String>,
}

impl LocalVolumeMemory {
    /// Takes in levels read from the settings file; an output playing now
    /// keeps the level it was found at, to put back when it stops.
    pub fn replace(&mut self, stored: LocalVolumes) {
        let playing = self.playing.iter();
        let kept: Vec<(String, f64)> = playing
            .filter_map(|id| Some((id.clone(), *self.stored.found.get(id)?)))
            .collect();
        self.stored = stored;
        self.stored.found.extend(kept);
    }

    pub fn playing(&self) -> impl Iterator<Item = &String> {
        self.playing.iter()
    }

    /// Output `id` starts playing; `current` is its level now, `None` when
    /// it can't be read. Returns the level to set before its first audio,
    /// `None` to leave it as found.
    pub fn switched_on(&mut self, id: &str, current: Option<f64>) -> Option<f64> {
        if !self.playing.insert(id.to_owned()) {
            return None;
        }
        let current = current?;
        let stored = &mut self.stored;
        // One a crash left set keeps the level it had before AudioSync.
        stored.found.entry(id.to_owned()).or_insert(current);
        let unchanged = stored
            .left
            .get(id)
            .is_some_and(|was| (was - current).abs() < SAME);
        match stored.remembered.get(id) {
            Some(&level) if unchanged => ((level - current).abs() >= SAME).then_some(level),
            _ => {
                stored.remembered.insert(id.to_owned(), current);
                None
            }
        }
    }

    /// The device refused the level `switched_on` returned: it keeps its
    /// own, so there is nothing to put back.
    pub fn refused(&mut self, id: &str) {
        self.stored.found.remove(id);
    }

    /// The device reports `level`: while the output plays, that is the
    /// level to remember.
    pub fn reported(&mut self, id: &str, level: f64) {
        if self.playing.contains(id) && self.stored.found.contains_key(id) {
            self.stored.remembered.insert(id.to_owned(), level);
        }
    }

    /// Output `id` stopped playing: returns the level to put back, `None`
    /// to leave it.
    pub fn switched_off(&mut self, id: &str) -> Option<f64> {
        if !self.playing.remove(id) {
            return None;
        }
        let found = self.stored.found.remove(id);
        match found {
            Some(level) => self.stored.left.insert(id.to_owned(), level),
            None => self.stored.left.remove(id),
        };
        found
    }

    /// At launch, before anything plays: the found levels a crash left on
    /// the outputs read now (`current`), to put back. One whose level was
    /// changed in Windows since keeps it; one not read waits.
    pub fn recover(&mut self, current: &BTreeMap<String, f64>) -> BTreeMap<String, f64> {
        let mut back = BTreeMap::new();
        let stored = &mut self.stored;
        let left: Vec<(String, f64)> = stored
            .found
            .iter()
            .filter(|(id, _)| !self.playing.contains(*id))
            .map(|(id, level)| (id.clone(), *level))
            .collect();
        for (id, level) in left {
            let Some(&now) = current.get(&id) else {
                continue;
            };
            stored.found.remove(&id);
            if stored
                .remembered
                .get(&id)
                .is_some_and(|kept| (kept - now).abs() < SAME)
            {
                back.insert(id.clone(), level);
                stored.left.insert(id, level);
            } else {
                stored.left.insert(id.clone(), now);
                stored.remembered.insert(id, now);
            }
        }
        back
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "headphones";

    fn levels(level: f64) -> BTreeMap<String, f64> {
        BTreeMap::from([(ID.to_owned(), level)])
    }

    // covers: S472
    #[test]
    fn the_first_switch_on_leaves_the_level_and_the_next_sets_the_one_last_played() {
        let mut memory = LocalVolumeMemory::default();
        assert_eq!(memory.switched_on(ID, Some(0.18)), None);
        memory.reported(ID, 0.6);
        assert_eq!(
            memory.switched_off(ID),
            Some(0.18),
            "the found level goes back"
        );
        assert_eq!(memory.switched_on(ID, Some(0.18)), Some(0.6));
        assert_eq!(
            memory.switched_on(ID, Some(0.18)),
            None,
            "once per switch-on"
        );
    }

    // covers: S472
    #[test]
    fn a_level_changed_in_windows_since_wins() {
        let mut memory = LocalVolumeMemory::default();
        memory.switched_on(ID, Some(0.18));
        memory.reported(ID, 0.6);
        memory.switched_off(ID);
        assert_eq!(memory.switched_on(ID, Some(0.4)), None);
        assert_eq!(memory.stored.remembered, levels(0.4));
    }

    // covers: S472
    #[test]
    fn a_refused_level_puts_nothing_back_and_an_unread_one_remembers_nothing() {
        let mut memory = LocalVolumeMemory::default();
        memory.switched_on(ID, Some(0.18));
        memory.reported(ID, 0.6);
        memory.switched_off(ID);
        assert_eq!(memory.switched_on(ID, Some(0.18)), Some(0.6));
        memory.refused(ID);
        assert_eq!(memory.switched_off(ID), None);
        let mut unread = LocalVolumeMemory::default();
        assert_eq!(unread.switched_on(ID, None), None);
        unread.reported(ID, 0.6);
        assert!(unread.stored.is_empty());
        assert_eq!(
            unread.switched_off(ID),
            None,
            "never played, so not stopped"
        );
    }

    // covers: S472
    #[test]
    fn a_crash_left_level_goes_back_at_launch_unless_changed_since() {
        let mut crashed = LocalVolumeMemory::default();
        crashed.switched_on(ID, Some(0.18));
        crashed.reported(ID, 0.6);
        let stored = crashed.stored.clone();

        let mut still = LocalVolumeMemory {
            stored: stored.clone(),
            ..LocalVolumeMemory::default()
        };
        assert_eq!(
            still.recover(&BTreeMap::new()),
            BTreeMap::new(),
            "not read: waits"
        );
        assert_eq!(still.recover(&levels(0.6)), levels(0.18));
        assert_eq!(still.switched_on(ID, Some(0.18)), Some(0.6));

        let mut changed = LocalVolumeMemory {
            stored,
            ..LocalVolumeMemory::default()
        };
        assert_eq!(changed.recover(&levels(0.3)), BTreeMap::new());
        assert_eq!(changed.switched_on(ID, Some(0.3)), None);
    }

    #[test]
    fn stored_levels_that_are_not_numbers_go_and_the_rest_stay_within_0_to_1() {
        let stored = LocalVolumes {
            remembered: BTreeMap::from([("a".to_owned(), f64::NAN), ("b".to_owned(), 1.5)]),
            found: levels(-0.2),
            left: BTreeMap::new(),
        }
        .clamped();
        assert_eq!(stored.remembered, BTreeMap::from([("b".to_owned(), 1.0)]));
        assert_eq!(stored.found, levels(0.0));
    }
}
