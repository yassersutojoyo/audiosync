//! Which Windows versions AudioSync runs on: Windows 11 and later.

/// A Windows version as the kernel reports it.
///
/// Fields are compared in declaration order, so `>=` compares whole versions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OsVersion {
    pub major: u32,
    pub minor: u32,
    pub build: u32,
}

/// The first Windows 11 build. Windows 11 still reports version 10.0, so only
/// the build number tells it apart from Windows 10.
pub const FIRST_WINDOWS_11_BUILD: u32 = 22000;

/// Shown before the app exits on an older Windows.
pub const UNSUPPORTED_MESSAGE: &str = "AudioSync needs Windows 11 or later.";

/// Whether AudioSync runs on `version`: Windows 11 (10.0.22000) or later.
pub fn is_supported(version: OsVersion) -> bool {
    version
        >= OsVersion {
            major: 10,
            minor: 0,
            build: FIRST_WINDOWS_11_BUILD,
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn version(major: u32, minor: u32, build: u32) -> OsVersion {
        OsVersion {
            major,
            minor,
            build,
        }
    }

    // covers: S389
    #[test]
    fn windows_10_22h2_is_refused() {
        assert!(!is_supported(version(10, 0, 19045)));
    }

    // covers: S389
    #[test]
    fn the_build_before_windows_11_is_refused() {
        assert!(!is_supported(version(10, 0, FIRST_WINDOWS_11_BUILD - 1)));
    }

    // covers: S389
    #[test]
    fn the_first_windows_11_build_runs() {
        assert!(is_supported(version(10, 0, 22000)));
    }

    // covers: S389
    #[test]
    fn windows_11_24h2_runs() {
        assert!(is_supported(version(10, 0, 26100)));
    }

    // covers: S389
    #[test]
    fn windows_7_is_refused() {
        assert!(!is_supported(version(6, 1, 7601)));
    }

    // covers: S389
    #[test]
    fn an_older_major_version_is_refused_whatever_its_build() {
        assert!(!is_supported(version(6, 3, 26100)));
    }

    // covers: S389
    #[test]
    fn a_later_major_version_runs() {
        assert!(is_supported(version(11, 0, 0)));
    }
}
