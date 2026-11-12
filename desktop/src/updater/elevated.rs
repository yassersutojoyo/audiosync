//! The update of an install for all users (spec S460): the install folder
//! under Program Files refuses the user's own swap, so the running exe
//! starts itself elevated to verify and swap the downloaded zip.

use std::path::{Path, PathBuf};

/// Whether a swap refused in `install_dir` goes through one elevation
/// prompt: only for a folder inside one of `machine_dirs`, so an install
/// for one user never prompts.
pub fn elevates(install_dir: &Path, machine_dirs: &[PathBuf]) -> bool {
    let _ = (install_dir, machine_dirs);
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine() -> Vec<PathBuf> {
        vec![
            PathBuf::from(r"C:\Program Files"),
            PathBuf::from(r"C:\Program Files (x86)"),
        ]
    }

    // covers: S460
    #[test]
    fn an_install_under_program_files_elevates() {
        assert!(elevates(
            Path::new(r"C:\Program Files\AudioSync"),
            &machine()
        ));
        assert!(elevates(
            Path::new(r"c:\PROGRAM FILES\AudioSync"),
            &machine()
        ));
        assert!(elevates(
            Path::new(r"C:\Program Files (x86)\AudioSync"),
            &machine()
        ));
    }

    // covers: S460
    #[test]
    fn an_install_for_one_user_never_elevates() {
        assert!(!elevates(
            Path::new(r"C:\Users\someone\AppData\Local\Programs\AudioSync"),
            &machine()
        ));
        assert!(!elevates(
            Path::new(r"C:\Program Files Extra\AudioSync"),
            &machine()
        ));
        assert!(!elevates(Path::new(r"C:\Program Files\AudioSync"), &[]));
    }
}
