//! The names the user gives speakers (spec S228).
//!
//! A speaker shows its system name (the one Windows or AirPlay discovery
//! reports) until renamed. Names are kept by speaker key (`lc:` or `ap:` and
//! the ID), so they outlive a reconnect and a restart, and they only change
//! what the windows show: the audio side never sees them.

use std::collections::BTreeMap;

/// The longest name kept, in characters.
pub const MAX_NAME_CHARS: usize = 60;

/// What the name field keeps while typing: at most [`MAX_NAME_CHARS`]
/// characters, untrimmed so a space can still be typed between words.
pub fn fitting(typed: &str) -> String {
    typed.chars().take(MAX_NAME_CHARS).collect()
}

/// A typed name as it is kept: trimmed, line breaks turned into spaces, cut
/// to [`MAX_NAME_CHARS`] characters.
pub fn normalized(typed: &str) -> String {
    let one_line: String = typed
        .chars()
        .map(|c| if c == '\r' || c == '\n' { ' ' } else { c })
        .collect();
    let cut = fitting(one_line.trim());
    cut.trim_end().to_owned()
}

/// The name a speaker shows once `typed` applies: the typed name, or the
/// system name when it is empty. The mock backend's rename.
#[cfg(any(test, feature = "mock-backend"))]
pub fn shown_after(typed: &str, system: &str) -> String {
    let name = normalized(typed);
    if name.is_empty() {
        system.to_owned()
    } else {
        name
    }
}

/// The custom names, by speaker key.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SpeakerNames {
    by_key: BTreeMap<String, String>,
}

impl SpeakerNames {
    /// The names a settings file kept; entries that aren't a name are
    /// skipped.
    pub fn from_saved(saved: &BTreeMap<String, String>) -> SpeakerNames {
        let by_key = saved
            .iter()
            .filter_map(|(key, name)| {
                let name = normalized(name);
                (!name.is_empty()).then(|| (key.clone(), name))
            })
            .collect();
        SpeakerNames { by_key }
    }

    /// The names as the settings file keeps them.
    pub fn saved(&self) -> BTreeMap<String, String> {
        self.by_key.clone()
    }

    /// What speaker `key` shows: its custom name, else `system`.
    pub fn shown(&self, key: &str, system: &str) -> String {
        self.by_key
            .get(key)
            .cloned()
            .unwrap_or_else(|| system.to_owned())
    }

    pub fn contains(&self, key: &str) -> bool {
        self.by_key.contains_key(key)
    }

    /// Renames speaker `key`. An empty name, or the system name typed again,
    /// brings back the system name and forgets the entry. Returns whether
    /// anything changed.
    pub fn rename(&mut self, key: &str, typed: &str, system: &str) -> bool {
        let name = normalized(typed);
        if name.is_empty() || name == system {
            self.by_key.remove(key).is_some()
        } else {
            self.by_key.insert(key.to_owned(), name.clone()).as_ref() != Some(&name)
        }
    }

    /// Speaker `from` is listed under the key `to` now: its name follows.
    pub fn move_key(&mut self, from: &str, to: &str) {
        if let Some(name) = self.by_key.remove(from) {
            self.by_key.entry(to.to_owned()).or_insert(name);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_typed_name_is_trimmed_kept_on_one_line_and_cut() {
        assert_eq!(normalized("  Desk\r\nLeft  "), "Desk  Left");
        assert_eq!(normalized(" \n "), "");
        let long = "é".repeat(80);
        assert_eq!(normalized(&long).chars().count(), MAX_NAME_CHARS);
        assert_eq!(shown_after("", "Speakers"), "Speakers");
        assert_eq!(shown_after(" Desk ", "Speakers"), "Desk");
    }

    #[test]
    fn set_clear_and_unknown_keys() {
        let mut names = SpeakerNames::default();
        assert_eq!(
            names.shown("lc:desk", "Speakers"),
            "Speakers",
            "unknown key"
        );

        assert!(names.rename("lc:desk", " Desk ", "Speakers"));
        assert_eq!(names.shown("lc:desk", "Speakers"), "Desk");
        assert!(
            !names.rename("lc:desk", "Desk", "Speakers"),
            "the same name again"
        );
        assert_eq!(
            names.shown("ap:kitchen", "Kitchen"),
            "Kitchen",
            "others keep theirs"
        );

        assert!(names.rename("lc:desk", "   ", "Speakers"), "empty clears");
        assert_eq!(names.shown("lc:desk", "Speakers"), "Speakers");
        assert!(names.saved().is_empty(), "with no entry left");

        assert!(names.rename("lc:desk", "Desk", "Speakers"));
        assert!(
            names.rename("lc:desk", "Speakers", "Speakers"),
            "the system name clears"
        );
        assert!(names.saved().is_empty());
        assert!(
            !names.rename("lc:gone", "", "Gone"),
            "clearing an unknown key changes nothing"
        );
    }

    #[test]
    fn names_survive_the_saved_form_and_follow_a_moved_speaker() {
        let mut names = SpeakerNames::default();
        names.rename("lc:desk", "Desk", "Speakers");
        let mut saved = names.saved();
        saved.insert("lc:blank".into(), " \n ".into());
        let read = SpeakerNames::from_saved(&saved);
        assert_eq!(read.shown("lc:desk", "Speakers"), "Desk");
        assert_eq!(read.saved().len(), 1, "a blank entry is skipped");

        let mut moved = read.clone();
        moved.move_key("lc:desk", "lc:desk-2");
        assert_eq!(moved.shown("lc:desk-2", "Speakers"), "Desk");
        assert_eq!(moved.shown("lc:desk", "Speakers"), "Speakers");
    }
}
