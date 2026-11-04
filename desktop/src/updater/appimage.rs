//! The Linux AppImage (spec S424): the file the update replaces, and the
//! check that a download is an AppImage for this arch.

use std::ffi::OsString;
use std::path::PathBuf;

use super::appcast::Arch;

/// The folder and file name of the AppImage in `appimage`, the `APPIMAGE`
/// variable the AppImage runtime sets to its own absolute path; `None` when
/// this copy wasn't started from one.
pub fn appimage_path(appimage: Option<OsString>) -> Option<(PathBuf, String)> {
    let path = PathBuf::from(appimage?);
    if !path.to_string_lossy().starts_with('/') {
        return None;
    }
    let name = path.file_name()?.to_str()?.to_owned();
    Some((path.parent()?.to_path_buf(), name))
}

/// Whether `bytes` is a 64-bit little-endian ELF for `arch` that carries
/// the AppImage type 2 mark (`AI` and 2 at offset 8).
pub fn fits(bytes: &[u8], arch: Arch) -> bool {
    bytes.len() >= 64
        && bytes.starts_with(b"\x7fELF")
        && bytes[4] == 2
        && bytes[5] == 1
        && bytes[8..11] == *b"AI\x02"
        && u16::from_le_bytes([bytes[18], bytes[19]]) == arch.elf_machine()
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// A 64-bit little-endian ELF header for `machine` with the AppImage
    /// type 2 mark, then `rest`.
    pub fn fake_appimage(machine: u16, rest: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0u8; 64];
        bytes[..4].copy_from_slice(b"\x7fELF");
        bytes[4] = 2;
        bytes[5] = 1;
        bytes[8..11].copy_from_slice(b"AI\x02");
        bytes[18..20].copy_from_slice(&machine.to_le_bytes());
        bytes.extend_from_slice(rest);
        bytes
    }

    /// [`fake_appimage`] stamped with `version` as `scripts/build_appimage.sh`
    /// stamps it: `AudioSyncVersion`, then the version padded with NULs to 16
    /// bytes, as the file's last 32 bytes.
    pub fn stamped(machine: u16, version: &str) -> Vec<u8> {
        let mut bytes = fake_appimage(machine, b"squashfs");
        bytes.extend_from_slice(b"AudioSyncVersion");
        let mut text = version.as_bytes().to_vec();
        text.resize(16, 0);
        bytes.extend_from_slice(&text);
        bytes
    }

    // covers: S424
    #[test]
    fn the_version_is_read_from_the_stamp_in_the_last_32_bytes() {
        use super::super::version::Version;
        assert_eq!(
            stamped_version(&stamped(62, "0.9.0")),
            Version::parse("0.9.0")
        );
        assert_eq!(
            stamped_version(&stamped(183, "0.10.12")),
            Version::parse("0.10.12")
        );
        assert_eq!(stamped_version(&fake_appimage(62, b"no stamp")), None);
        let mut no_mark = stamped(62, "0.9.0");
        let at = no_mark.len() - 32;
        no_mark[at] = b'a';
        assert_eq!(stamped_version(&no_mark), None);
        let mut not_dotted = stamped(62, "0.9.0");
        let at = not_dotted.len() - 16;
        not_dotted[at] = b'x';
        assert_eq!(stamped_version(&not_dotted), None);
        assert_eq!(stamped_version(&[]), None);
    }

    // covers: S424
    #[test]
    fn only_an_absolute_appimage_path_is_replaced_in_place() {
        assert_eq!(
            appimage_path(Some(OsString::from(
                "/home/me/Apps/AudioSync-0.8.0-x86_64.AppImage"
            ))),
            Some((
                PathBuf::from("/home/me/Apps"),
                "AudioSync-0.8.0-x86_64.AppImage".to_owned()
            ))
        );
        for unusable in [None, Some(""), Some("AudioSync.AppImage"), Some("/")] {
            assert_eq!(
                appimage_path(unusable.map(OsString::from)),
                None,
                "{unusable:?}"
            );
        }
    }

    // covers: S424
    #[test]
    fn an_appimage_must_be_a_64_bit_elf_of_this_arch_with_the_appimage_mark() {
        assert!(fits(&fake_appimage(62, b"x"), Arch::X64));
        assert!(fits(&fake_appimage(183, b"x"), Arch::Arm64));
        assert!(!fits(&fake_appimage(183, b"x"), Arch::X64));
        assert!(!fits(&fake_appimage(62, b"x"), Arch::Arm64));
        let mut unmarked = fake_appimage(62, b"x");
        unmarked[10] = 1;
        assert!(!fits(&unmarked, Arch::X64));
        let mut elf32 = fake_appimage(62, b"x");
        elf32[4] = 1;
        assert!(!fits(&elf32, Arch::X64));
        assert!(!fits(b"MZ not an ELF", Arch::X64));
        assert!(!fits(&[], Arch::X64));
    }
}
