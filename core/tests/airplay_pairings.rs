//! The Mac's store of AirPlay pairing credentials (`airplay_pairings`):
//! a round trip through the file, a corrupt or newer file ignored, the
//! file for its owner only, and no secret in anything printable.

use std::fs;
use std::path::PathBuf;

use audiosync_airplay::pairing::{Credentials, SECRET_BYTES};
use audiosync_core::airplay_pairings::{Loaded, MAGIC, PairingStore, VERSION};

/// Credentials with a recognizable client ID and secret.
fn credentials(client: &str, fill: u8) -> Credentials {
    Credentials::new(client, [fill; SECRET_BYTES]).expect("valid credentials")
}

/// A fresh folder for one test, in the target folder's scratch space.
fn folder(name: &str) -> PathBuf {
    let folder = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join("airplay-pairings")
        .join(name);
    let _ = fs::remove_dir_all(&folder);
    folder
}

#[test]
fn credentials_survive_a_restart() {
    let path = folder("restart")
        .join("AudioSync")
        .join("airplay-pairings.dat");
    let (mut store, loaded) = PairingStore::open(path.clone());
    assert_eq!(loaded, Loaded::NoFile);
    assert!(store.is_empty());
    store
        .insert("02005E005301", credentials("0123456789ABCDEF", 7))
        .expect("written");
    store
        .insert("raop:Den TV", credentials("FEDCBA9876543210", 9))
        .expect("written");
    assert!(path.exists(), "the folder is created and the file written");
    assert!(
        !path.with_extension("dat.tmp").exists(),
        "no temporary file stays behind"
    );

    let (again, loaded) = PairingStore::open(path.clone());
    assert_eq!(loaded, Loaded::Receivers(2));
    assert_eq!(again.len(), 2);
    assert_eq!(
        again.get("02005E005301"),
        Some(&credentials("0123456789ABCDEF", 7))
    );
    assert_eq!(
        again.get("raop:Den TV"),
        Some(&credentials("FEDCBA9876543210", 9))
    );
    assert!(!again.contains("02005E005399"));

    let bytes = fs::read(&path).expect("the file");
    assert!(bytes.starts_with(MAGIC) && bytes[MAGIC.len()] == VERSION);
}

#[test]
fn forgetting_the_last_pairing_removes_the_file() {
    let path = folder("forget").join("airplay-pairings.dat");
    let (mut store, _) = PairingStore::open(path.clone());
    store
        .insert("02005E005301", credentials("0123456789ABCDEF", 7))
        .expect("written");
    assert!(!store.remove("02005E005399").expect("nothing to write"));
    assert!(path.exists());
    assert!(store.remove("02005E005301").expect("written"));
    assert!(!path.exists(), "nothing paired, no file");
    let (again, loaded) = PairingStore::open(path);
    assert_eq!(loaded, Loaded::NoFile);
    assert!(again.is_empty());
}

#[test]
fn a_corrupt_file_is_ignored_and_replaced_by_the_next_pairing() {
    let base = folder("corrupt");
    fs::create_dir_all(&base).expect("folder");
    let path = base.join("airplay-pairings.dat");
    let mut good = MAGIC.to_vec();
    good.push(VERSION);
    let cases: Vec<Vec<u8>> = vec![
        b"not a pairings file".to_vec(),
        MAGIC.to_vec(),
        [good.clone(), vec![5, 0, b'a']].concat(),
        [good.clone(), vec![1, 0, b'a', 3, 0, 1, 2, 3]].concat(),
        [MAGIC.to_vec(), vec![0]].concat(),
    ];
    for bytes in cases {
        fs::write(&path, &bytes).expect("written");
        let (store, loaded) = PairingStore::open(path.clone());
        assert!(matches!(loaded, Loaded::Ignored(_)), "{loaded:?}");
        assert!(store.is_empty());
    }
    let (mut store, _) = PairingStore::open(path.clone());
    store
        .insert("02005E005301", credentials("0123456789ABCDEF", 7))
        .expect("the corrupt file is replaced");
    assert_eq!(PairingStore::open(path).1, Loaded::Receivers(1));
}

#[test]
fn a_newer_file_is_ignored_and_left_as_it_is() {
    let base = folder("newer");
    fs::create_dir_all(&base).expect("folder");
    let path = base.join("airplay-pairings.dat");
    let newer = [MAGIC, &[VERSION + 1], b"whatever comes next"].concat();
    fs::write(&path, &newer).expect("written");
    let (mut store, loaded) = PairingStore::open(path.clone());
    assert_eq!(loaded, Loaded::Newer(VERSION + 1));
    assert!(store.is_empty());
    assert!(
        store
            .insert("02005E005301", credentials("0123456789ABCDEF", 7))
            .is_err(),
        "the newer file isn't overwritten"
    );
    assert!(store.contains("02005E005301"), "kept for this run");
    assert_eq!(fs::read(&path).expect("still there"), newer);
}

#[test]
fn a_leftover_temporary_file_does_not_block_a_write() {
    let base = folder("leftover");
    fs::create_dir_all(&base).expect("folder");
    let path = base.join("airplay-pairings.dat");
    fs::write(base.join("airplay-pairings.dat.tmp"), b"from a crash").expect("written");
    let (mut store, _) = PairingStore::open(path.clone());
    store
        .insert("02005E005301", credentials("0123456789ABCDEF", 7))
        .expect("written");
    assert!(!base.join("airplay-pairings.dat.tmp").exists());
    assert_eq!(PairingStore::open(path).1, Loaded::Receivers(1));
}

#[cfg(unix)]
#[test]
fn the_file_is_for_its_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let path = folder("mode")
        .join("AudioSync")
        .join("airplay-pairings.dat");
    let (mut store, _) = PairingStore::open(path.clone());
    store
        .insert("02005E005301", credentials("0123456789ABCDEF", 7))
        .expect("written");
    let mode = fs::metadata(&path).expect("the file").permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let folder_mode = fs::metadata(path.parent().expect("its folder"))
        .expect("the folder")
        .permissions()
        .mode();
    assert_eq!(folder_mode & 0o077, 0, "the new folder is its owner's only");
}

#[test]
fn nothing_printable_holds_a_secret() {
    let path = folder("debug").join("airplay-pairings.dat");
    let (mut store, _) = PairingStore::open(path.clone());
    store
        .insert("02005E005301", credentials("0123456789ABCDEF", 0xAB))
        .expect("written");
    let shown = format!("{store:?} {}", PairingStore::open(path).1);
    assert!(shown.contains("02005E005301"));
    assert!(!shown.contains("0123456789ABCDEF") && !shown.to_lowercase().contains("ab, ab"));
}

#[test]
fn an_in_memory_store_writes_nothing() {
    let mut store = PairingStore::in_memory();
    store
        .insert("02005E005301", credentials("0123456789ABCDEF", 7))
        .expect("nothing to write");
    assert!(store.contains("02005E005301"));
    assert!(store.remove("02005E005301").expect("nothing to write"));
}

// covers: S224
#[cfg(unix)]
#[test]
fn a_file_others_may_read_is_made_owner_only_and_still_used() {
    use std::os::unix::fs::PermissionsExt;
    let path = folder("open-to-others").join("airplay-pairings.dat");
    let (mut store, _) = PairingStore::open(path.clone());
    assert!(!store.tightened());
    store
        .insert("02005E005301", credentials("0123456789ABCDEF", 3))
        .expect("written");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("mode set");

    let (store, loaded) = PairingStore::open(path.clone());
    assert_eq!(loaded, Loaded::Receivers(1));
    assert!(store.tightened(), "the host is told, so it can log it");
    let mode = fs::metadata(&path).expect("metadata").permissions().mode();
    assert_eq!(mode & 0o777, 0o600);

    let (store, loaded) = PairingStore::open(path);
    assert_eq!(loaded, Loaded::Receivers(1));
    assert!(!store.tightened(), "an owner-only file is left as it is");
}
