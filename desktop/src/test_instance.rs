//! `AUDIOSYNC_INSTANCE=<name>`: a test copy that runs beside the user's own
//! copy, with its own single-instance names, its own data folder and its
//! name on its title and tooltip. Honoured in debug builds, and in release
//! builds only next to `AUDIOSYNC_DEBUG=1`, so a user's launch never sees it.

use std::ffi::OsStr;
use std::sync::OnceLock;

/// The environment variable that names a test copy.
pub const ENV_VAR: &str = "AUDIOSYNC_INSTANCE";

/// The test copy's name for these environment values, `None` for the
/// user's own copy. Only ASCII letters, digits, `-` and `_`, at most 32 of
/// them, since the name goes into kernel object and folder names.
pub fn name_for(
    instance: Option<&OsStr>,
    debug: Option<&OsStr>,
    debug_build: bool,
) -> Option<String> {
    if !debug_build && debug != Some(OsStr::new("1")) {
        return None;
    }
    let name = instance?.to_str()?;
    let valid = !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    valid.then(|| name.to_ascii_lowercase())
}

/// This launch's test copy name, read once.
pub fn current() -> Option<&'static str> {
    static NAME: OnceLock<Option<String>> = OnceLock::new();
    NAME.get_or_init(|| {
        name_for(
            std::env::var_os(ENV_VAR).as_deref(),
            std::env::var_os(crate::debug_log::ENV_VAR).as_deref(),
            cfg!(debug_assertions),
        )
    })
    .as_deref()
}

/// `base` for the user's copy, `base-<name>` for a test copy: the
/// single-instance names and the data folder.
pub fn qualified(base: &str, instance: Option<&str>) -> String {
    match instance {
        Some(name) => format!("{base}-{name}"),
        None => base.to_owned(),
    }
}

/// The suffix the title and tooltip carry, empty for the user's copy.
pub fn label(instance: Option<&str>) -> String {
    instance
        .map(|name| format!(" [{name}]"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(value: &str, debug: Option<&str>, debug_build: bool) -> Option<String> {
        name_for(Some(OsStr::new(value)), debug.map(OsStr::new), debug_build)
    }

    #[test]
    fn two_test_copies_get_two_lock_names_and_two_data_folders() {
        let a = name("a", None, true);
        let b = name("b", None, true);
        let lock = |n: &Option<String>| qualified("Local\\app.instance", n.as_deref());
        let folder = |n: &Option<String>| qualified("AudioSync", n.as_deref());
        assert_ne!(lock(&a), lock(&b));
        assert_ne!(folder(&a), folder(&b));
        assert_ne!(lock(&a), lock(&None));
        assert_ne!(folder(&a), folder(&None));
        assert_eq!(folder(&None), "AudioSync");
        assert_eq!(label(a.as_deref()), " [a]");
        assert_eq!(label(None), "");
    }

    #[test]
    fn a_release_build_ignores_the_variable_without_the_debug_log() {
        assert_eq!(name("test", None, false), None);
        assert_eq!(name("test", Some("0"), false), None);
        assert_eq!(name("test", Some("1"), false).as_deref(), Some("test"));
        assert_eq!(name("test", None, true).as_deref(), Some("test"));
        assert_eq!(name_for(None, Some(OsStr::new("1")), true), None);
    }

    #[test]
    fn a_name_that_cannot_go_into_a_kernel_object_or_folder_name_is_ignored() {
        for bad in ["", "a\\b", "a/b", "..", "a b", &"x".repeat(33)] {
            assert_eq!(name(bad, None, true), None, "{bad:?}");
        }
        assert_eq!(name("qa_2-x", None, true).as_deref(), Some("qa_2-x"));
        assert_eq!(name("QA", None, true), name("qa", None, true));
    }
}
