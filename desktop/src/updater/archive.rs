//! Unzipping a verified download inside its temp folder (spec S300).

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use super::appcast::Arch;
use super::files::{Files, is_disk_full};

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
            ArchiveError::Io(error) => write!(f, "{error}"),
        }
    }
}

impl From<io::Error> for ArchiveError {
    fn from(error: io::Error) -> ArchiveError {
        if is_disk_full(&error) {
            ArchiveError::DiskFull
        } else {
            ArchiveError::Io(error)
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Unpacked {
    pub exe: PathBuf,
    pub notices: Option<PathBuf>,
}

/// Whether `name` is a plain file name at the top of the archive.
pub fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && !name.contains(['/', '\\', ':'])
        && !name.contains("..")
        && name.trim() == name
        && name != "."
}

/// Unpacks `zip` into `dir`: the one `audiosync.exe` and the notices, if
/// there, checked before anything is written.
pub fn unpack(
    zip: &Path,
    dir: &Path,
    arch: Arch,
    files: &dyn Files,
) -> Result<Unpacked, ArchiveError> {
    let mut archive = zip::ZipArchive::new(File::open(zip)?)
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
    let exe_path = dir.join(EXE_NAME);
    write_entry(&mut archive, exe, &exe_path, &mut budget, files)?;
    if pe_machine(&exe_path)? != Some(arch.pe_machine()) {
        return Err(ArchiveError::WrongMachine);
    }
    let notices = match notices {
        Some(index) => {
            let path = dir.join(NOTICES_NAME);
            write_entry(&mut archive, index, &path, &mut budget, files)?;
            Some(path)
        }
        None => None,
    };
    Ok(Unpacked {
        exe: exe_path,
        notices,
    })
}

fn write_entry(
    archive: &mut zip::ZipArchive<File>,
    index: usize,
    path: &Path,
    budget: &mut u64,
    files: &dyn Files,
) -> Result<(), ArchiveError> {
    let entry = archive
        .by_index(index)
        .map_err(|error| ArchiveError::NotAZip(error.to_string()))?;
    let mut out = files.create(path, true)?;
    let written = io::copy(&mut entry.take(*budget + 1), &mut out)?;
    if written > *budget {
        return Err(ArchiveError::TooBig);
    }
    *budget -= written;
    out.flush()?;
    out.sync()?;
    Ok(())
}

/// The machine field of the PE image at `path`, or `None` when it isn't one.
pub fn pe_machine(path: &Path) -> io::Result<Option<u16>> {
    let mut head = Vec::new();
    File::open(path)?.take(4096).read_to_end(&mut head)?;
    if head.get(..2) != Some(b"MZ") {
        return Ok(None);
    }
    let Some(offset) = head
        .get(0x3C..0x40)
        .map(|bytes| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize)
    else {
        return Ok(None);
    };
    if head.get(offset..offset + 4) != Some(b"PE\0\0") {
        return Ok(None);
    }
    Ok(head
        .get(offset + 4..offset + 6)
        .map(|bytes| u16::from_le_bytes([bytes[0], bytes[1]])))
}

#[cfg(test)]
pub mod tests {
    use zip::write::SimpleFileOptions;

    use super::*;
    use crate::updater::files::RealFiles;
    use crate::updater::tests::test_dir;

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

    fn unpack_bytes(name: &str, zip: &[u8]) -> (PathBuf, Result<Unpacked, ArchiveError>) {
        let dir = test_dir(name);
        let path = dir.join("download.zip");
        std::fs::write(&path, zip).expect("the zip is written");
        let result = unpack(&path, &dir, Arch::X64, &RealFiles);
        (dir, result)
    }

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
            let (dir, result) = unpack_bytes("bad-name", &zip);
            assert!(
                matches!(result, Err(ArchiveError::BadName(_))),
                "{bad:?}: {result:?}"
            );
            assert!(!dir.join(EXE_NAME).exists(), "{bad:?} wrote nothing");
        }
        assert!(!plain_name(""));
        assert!(plain_name("README.md"));
    }

    #[test]
    fn a_zip_without_audiosync_exe_or_with_an_exe_for_the_other_machine_is_refused() {
        let (_, result) = unpack_bytes("no-exe", &zip_of(&[(NOTICES_NAME, b"n")]));
        assert!(matches!(result, Err(ArchiveError::NoExe)), "{result:?}");

        let arm = fake_exe(Arch::Arm64.pe_machine(), b"");
        let (_, result) = unpack_bytes("other-machine", &zip_of(&[(EXE_NAME, &arm)]));
        assert!(
            matches!(result, Err(ArchiveError::WrongMachine)),
            "{result:?}"
        );

        let (_, result) = unpack_bytes("not-pe", &zip_of(&[(EXE_NAME, b"plain text")]));
        assert!(
            matches!(result, Err(ArchiveError::WrongMachine)),
            "{result:?}"
        );

        let x64 = fake_exe(Arch::X64.pe_machine(), b"");
        let two = zip_of(&[(EXE_NAME, &x64), ("AudioSync.EXE", &x64)]);
        let (_, result) = unpack_bytes("two-exes", &two);
        assert!(matches!(result, Err(ArchiveError::TwoExes)), "{result:?}");

        let (_, result) = unpack_bytes("not-a-zip", b"PK but not really");
        assert!(
            matches!(result, Err(ArchiveError::NotAZip(_))),
            "{result:?}"
        );

        let good = zip_of(&[
            (EXE_NAME, &x64),
            (NOTICES_NAME, b"n"),
            ("extra.txt", b"skip"),
        ]);
        let (dir, result) = unpack_bytes("good", &good);
        let unpacked = result.expect("a good zip unpacks");
        assert_eq!(unpacked.notices, Some(dir.join(NOTICES_NAME)));
        assert!(!dir.join("extra.txt").exists(), "other entries are skipped");
    }
}
