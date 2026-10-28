//! Unzipping a verified download in memory (spec S300).

use std::io::{self, Read};

use super::appcast::Arch;
use super::files::{is_blocked, is_disk_full};
use super::version::Version;

pub const EXE_NAME: &str = "audiosync.exe";
pub const NOTICES_NAME: &str = "THIRD-PARTY-NOTICES.txt";
pub const MAX_UNPACKED: u64 = 256 * 1024 * 1024;

#[derive(Debug)]
pub enum ArchiveError {
    NotAZip(String),
    BadName(String),
    NoExe,
    TwoExes,
    TooBig,
    /// The exe isn't a PE image for this machine.
    WrongMachine,
    DiskFull,
    /// Windows security removed or blocked a file (spec S315).
    Blocked,
    Io(io::Error),
}

impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArchiveError::NotAZip(why) => write!(f, "not a zip ({why})"),
            ArchiveError::BadName(name) => write!(f, "an entry named {name:?}"),
            ArchiveError::NoExe => write!(f, "no {EXE_NAME}"),
            ArchiveError::TwoExes => write!(f, "two {EXE_NAME} entries"),
            ArchiveError::TooBig => write!(f, "over {MAX_UNPACKED} bytes unpacked"),
            ArchiveError::WrongMachine => write!(f, "an exe for another machine"),
            ArchiveError::DiskFull => write!(f, "the disk is full"),
            ArchiveError::Blocked => write!(f, "blocked by Windows security"),
            ArchiveError::Io(error) => write!(f, "{error}"),
        }
    }
}

impl From<io::Error> for ArchiveError {
    fn from(error: io::Error) -> ArchiveError {
        if is_disk_full(&error) {
            ArchiveError::DiskFull
        } else if is_blocked(&error) {
            ArchiveError::Blocked
        } else {
            ArchiveError::Io(error)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Unpacked {
    pub exe: Vec<u8>,
    pub notices: Option<Vec<u8>>,
}

/// Whether `name` is a plain file name at the top of the archive.
pub fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['/', '\\', ':'])
        && !name.contains("..")
        && name.trim() == name
        && name != "."
}

/// Unpacks the verified `zip` bytes in memory: the one `audiosync.exe` and
/// the notices, if there, checked before either is read.
pub fn unpack(zip: &[u8], arch: Arch) -> Result<Unpacked, ArchiveError> {
    let mut archive = zip::ZipArchive::new(io::Cursor::new(zip))
        .map_err(|error| ArchiveError::NotAZip(error.to_string()))?;
    let mut exe = None;
    let mut notices = None;
    for index in 0..archive.len() {
        let entry = archive
            .by_index_raw(index)
            .map_err(|error| ArchiveError::NotAZip(error.to_string()))?;
        let name = String::from_utf8_lossy(entry.name_raw()).into_owned();
        if !plain_name(&name) {
            return Err(ArchiveError::BadName(name));
        }
        if name.eq_ignore_ascii_case(EXE_NAME) {
            if exe.replace(index).is_some() {
                return Err(ArchiveError::TwoExes);
            }
        } else if name.eq_ignore_ascii_case(NOTICES_NAME) {
            notices = Some(index);
        }
    }
    let exe = exe.ok_or(ArchiveError::NoExe)?;
    let mut budget = MAX_UNPACKED;
    let exe = read_entry(&mut archive, exe, &mut budget)?;
    if pe_machine(&exe) != Some(arch.pe_machine()) {
        return Err(ArchiveError::WrongMachine);
    }
    let notices = match notices {
        Some(index) => Some(read_entry(&mut archive, index, &mut budget)?),
        None => None,
    };
    Ok(Unpacked { exe, notices })
}

fn read_entry(
    archive: &mut zip::ZipArchive<io::Cursor<&[u8]>>,
    index: usize,
    budget: &mut u64,
) -> Result<Vec<u8>, ArchiveError> {
    let entry = archive
        .by_index(index)
        .map_err(|error| ArchiveError::NotAZip(error.to_string()))?;
    let mut bytes = Vec::new();
    let read = entry.take(*budget + 1).read_to_end(&mut bytes)? as u64;
    if read > *budget {
        return Err(ArchiveError::TooBig);
    }
    *budget -= read;
    Ok(bytes)
}

/// The machine field of the PE image `exe`, or `None` when it isn't one.
pub fn pe_machine(exe: &[u8]) -> Option<u16> {
    let head = &exe[..exe.len().min(4096)];
    if head.get(..2) != Some(b"MZ") {
        return None;
    }
    let offset = head
        .get(0x3C..0x40)
        .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)?;
    if head.get(offset..offset + 4) != Some(b"PE\0\0") {
        return None;
    }
    head.get(offset + 4..offset + 6)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]]))
}

/// The product version in `exe`'s version resource (`VS_FIXEDFILEINFO`), or
/// `None` without one.
pub fn product_version(exe: &[u8]) -> Option<Version> {
    let key: Vec<u8> = "VS_VERSION_INFO\0"
        .encode_utf16()
        .flat_map(u16::to_le_bytes)
        .collect();
    let word = |at: usize| {
        exe.get(at..at + 4)
            .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    };
    // The fixed part follows the key at the next 32-bit boundary.
    exe.windows(key.len())
        .enumerate()
        .filter(|(_, window)| *window == key.as_slice())
        .map(|(at, _)| (at + key.len()).next_multiple_of(4))
        .find(|&fixed| word(fixed) == Some(0xFEEF_04BD))
        .and_then(|fixed| {
            let (high, low) = (word(fixed + 16)?, word(fixed + 20)?);
            Some(Version::from_parts(
                &[high >> 16, high & 0xFFFF, low >> 16, low & 0xFFFF].map(u64::from),
            ))
        })
}

#[cfg(test)]
pub mod tests {
    use zip::write::SimpleFileOptions;

    use std::io::Write;

    use super::*;

    /// The first bytes of a PE image for `machine`, enough for the check.
    pub fn fake_exe(machine: u16, marker: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0u8; 0x80];
        bytes[..2].copy_from_slice(b"MZ");
        bytes[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        bytes[0x40..0x44].copy_from_slice(b"PE\0\0");
        bytes[0x44..0x46].copy_from_slice(&machine.to_le_bytes());
        bytes.extend_from_slice(marker);
        bytes
    }

    /// An x64 PE image whose version resource reports `version` as its
    /// product version, laid out as the resource compiler writes it.
    pub fn fake_exe_reporting(version: [u16; 4], marker: &[u8]) -> Vec<u8> {
        let mut bytes = fake_exe(Arch::X64.pe_machine(), b"");
        bytes.extend([0u8; 6]);
        bytes.extend(
            "VS_VERSION_INFO\0"
                .encode_utf16()
                .flat_map(u16::to_le_bytes),
        );
        bytes.resize(bytes.len().next_multiple_of(4), 0);
        let pair = |high: u16, low: u16| ((u32::from(high) << 16) | u32::from(low)).to_le_bytes();
        for word in [
            0xFEEF_04BD_u32.to_le_bytes(),
            0x0001_0000_u32.to_le_bytes(),
            pair(version[0], version[1]),
            pair(version[2], version[3]),
            pair(version[0], version[1]),
            pair(version[2], version[3]),
        ] {
            bytes.extend(word);
        }
        bytes.extend_from_slice(marker);
        bytes
    }

    pub fn zip_of(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(io::Cursor::new(Vec::new()));
        for (name, data) in entries {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .expect("an entry starts");
            writer.write_all(data).expect("an entry is written");
        }
        writer.finish().expect("the zip finishes").into_inner()
    }

    fn unpack_bytes(zip: &[u8]) -> Result<Unpacked, ArchiveError> {
        unpack(zip, Arch::X64)
    }

    // covers: S300
    #[test]
    fn zip_entries_with_folders_dot_dot_or_drive_letters_are_refused() {
        let exe = fake_exe(Arch::X64.pe_machine(), b"new");
        for bad in [
            "bin/audiosync.exe",
            "../audiosync.exe",
            "..",
            "C:audiosync.exe",
            "/audiosync.exe",
            "\\audiosync.exe",
            "a\\b.txt",
            "folder/",
        ] {
            let zip = zip_of(&[("audiosync.exe", &exe), (bad, b"x")]);
            let result = unpack_bytes(&zip);
            assert!(
                matches!(result, Err(ArchiveError::BadName(_))),
                "{bad:?}: {result:?}"
            );
        }
        assert!(!plain_name(""));
        assert!(plain_name("README.md"));
    }

    // covers: S300
    #[test]
    fn a_zip_without_audiosync_exe_or_with_an_exe_for_the_other_machine_is_refused() {
        let result = unpack_bytes(&zip_of(&[(NOTICES_NAME, b"n")]));
        assert!(matches!(result, Err(ArchiveError::NoExe)), "{result:?}");

        let arm = fake_exe(Arch::Arm64.pe_machine(), b"");
        let result = unpack_bytes(&zip_of(&[(EXE_NAME, &arm)]));
        assert!(
            matches!(result, Err(ArchiveError::WrongMachine)),
            "{result:?}"
        );

        let result = unpack_bytes(&zip_of(&[(EXE_NAME, b"plain text")]));
        assert!(
            matches!(result, Err(ArchiveError::WrongMachine)),
            "{result:?}"
        );

        let x64 = fake_exe(Arch::X64.pe_machine(), b"");
        let two = zip_of(&[(EXE_NAME, &x64), ("AudioSync.EXE", &x64)]);
        let result = unpack_bytes(&two);
        assert!(matches!(result, Err(ArchiveError::TwoExes)), "{result:?}");

        let result = unpack_bytes(b"PK but not really");
        assert!(
            matches!(result, Err(ArchiveError::NotAZip(_))),
            "{result:?}"
        );

        let good = zip_of(&[
            (EXE_NAME, &x64),
            (NOTICES_NAME, b"n"),
            ("extra.txt", b"skip"),
        ]);
        let unpacked = unpack_bytes(&good).expect("a good zip unpacks");
        assert_eq!(unpacked.exe, x64);
        assert_eq!(unpacked.notices.as_deref(), Some(&b"n"[..]));
    }
}
