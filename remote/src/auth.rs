//! The pairing code, the limits on guessing it, and the paired devices.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Digits in a pairing code.
pub const CODE_DIGITS: usize = 6;
/// Wrong codes one address may send within [`Limits::window`] before it gets 429.
pub const WRONG_PER_WINDOW: usize = 5;
/// Wrong codes in total after which pairing stays locked until the code is rotated.
pub const WRONG_BEFORE_LOCK: u32 = 20;
/// Devices remembered at once; pairing another drops the oldest.
pub const MAX_DEVICES: usize = 8;
/// The paired-device file's format.
const FILE_VERSION: u64 = 1;

/// No randomness for a code or a token; a guessable one is never used instead.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RandomError;

fn random_bytes<const N: usize>() -> Result<[u8; N], RandomError> {
    let mut bytes = [0u8; N];
    getrandom::fill(&mut bytes).map_err(|_| RandomError)?;
    Ok(bytes)
}

/// A uniform 6-digit code, leading zeros kept.
pub fn random_code() -> Result<String, RandomError> {
    // The largest multiple of 10^6 below 2^32, so every code is equally likely.
    const LIMIT: u32 = 4_294_000_000;
    loop {
        let value = u32::from_le_bytes(random_bytes()?);
        if value < LIMIT {
            return Ok(format!("{:06}", value % 1_000_000));
        }
    }
}

fn equal_in_constant_time(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The time limits of pairing; the defaults are the product's, tests shorten them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// How long a code works after it is made.
    pub code_lifetime: Duration,
    /// The window [`WRONG_PER_WINDOW`] counts in.
    pub window: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            code_lifetime: Duration::from_secs(10 * 60),
            window: Duration::from_secs(60),
        }
    }
}

/// What a pairing try came to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PairOutcome {
    /// The code was right; it is used up and a new one made.
    Paired,
    Wrong,
    /// No code works now: it expired or none could be made.
    Expired,
    /// This address sent [`WRONG_PER_WINDOW`] wrong codes within the window.
    Limited,
    /// [`WRONG_BEFORE_LOCK`] wrong codes in total; rotating the code unlocks.
    Locked,
}

/// The current pairing code and the count of wrong tries.
#[derive(Debug)]
pub struct Pairing {
    limits: Limits,
    code: Option<(String, Instant)>,
    wrong_total: u32,
    recent_wrong: HashMap<IpAddr, VecDeque<Instant>>,
}

impl Pairing {
    pub fn new(limits: Limits, now: Instant) -> Result<Pairing, RandomError> {
        let mut pairing = Pairing {
            limits,
            code: None,
            wrong_total: 0,
            recent_wrong: HashMap::new(),
        };
        pairing.rotate(now)?;
        Ok(pairing)
    }

    /// A new code, as when the user reopens the setting: the old one stops
    /// working, the lockout and the counts are cleared.
    pub fn rotate(&mut self, now: Instant) -> Result<String, RandomError> {
        self.wrong_total = 0;
        self.recent_wrong.clear();
        self.code = None;
        let code = random_code()?;
        self.code = Some((code.clone(), now));
        Ok(code)
    }

    /// The code that works now, if any.
    pub fn current(&self, now: Instant) -> Option<&str> {
        self.code
            .as_ref()
            .filter(|(_, made)| now.saturating_duration_since(*made) < self.limits.code_lifetime)
            .map(|(code, _)| code.as_str())
    }

    pub fn locked(&self) -> bool {
        self.wrong_total >= WRONG_BEFORE_LOCK
    }

    /// One try from `from`. A limited or locked try is refused before the
    /// code is looked at, so the right code doesn't get through either.
    pub fn attempt(&mut self, from: IpAddr, code: &str, now: Instant) -> PairOutcome {
        if self.locked() {
            return PairOutcome::Locked;
        }
        let window = self.limits.window;
        self.recent_wrong.retain(|_, times| {
            while times
                .front()
                .is_some_and(|time| now.saturating_duration_since(*time) >= window)
            {
                times.pop_front();
            }
            !times.is_empty()
        });
        if self
            .recent_wrong
            .get(&from)
            .is_some_and(|times| times.len() >= WRONG_PER_WINDOW)
        {
            return PairOutcome::Limited;
        }
        let outcome = match self.current(now) {
            Some(current) if equal_in_constant_time(current.as_bytes(), code.trim().as_bytes()) => {
                // Single use: the next device needs the next code.
                self.code = random_code().ok().map(|code| (code, now));
                return PairOutcome::Paired;
            }
            Some(_) => PairOutcome::Wrong,
            None => PairOutcome::Expired,
        };
        self.wrong_total = self.wrong_total.saturating_add(1);
        self.recent_wrong.entry(from).or_default().push_back(now);
        outcome
    }
}

/// A device token: 128 random bits as 32 hex digits.
pub fn random_token() -> Result<String, RandomError> {
    let bytes: [u8; 16] = random_bytes()?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

type TokenHash = [u8; 32];

fn token_hash(token: &str) -> TokenHash {
    Sha256::digest(token.as_bytes()).into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn from_hex(text: &str) -> Option<TokenHash> {
    if text.len() != 64 || !text.is_ascii() {
        return None;
    }
    let mut hash = [0u8; 32];
    for (index, byte) in hash.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(hash)
}

/// The paired devices: only each token's SHA-256, oldest first. A change
/// returns a [`Snapshot`] that the caller saves after letting go of its
/// lock, so a slow disk never holds up a reader.
#[derive(Debug)]
pub struct TokenStore {
    path: Option<PathBuf>,
    hashes: Vec<TokenHash>,
    generation: u64,
}

/// Why the paired-device file was not read at start; the store starts empty.
#[derive(Debug)]
pub enum LoadProblem {
    Unreadable(io::Error),
    NotInFormat,
}

/// The devices as one change left them, for [`DeviceFile::save`].
#[derive(Debug)]
#[must_use = "a change reaches the file only once its snapshot is saved"]
pub struct Snapshot {
    path: Option<PathBuf>,
    hashes: Vec<TokenHash>,
    generation: u64,
}

/// Writes snapshots to the paired-device file one at a time; one older
/// than the last written is skipped, so the file never goes back.
#[derive(Debug, Default)]
pub struct DeviceFile {
    written: Mutex<u64>,
}

impl DeviceFile {
    pub fn save(&self, snapshot: Snapshot) -> io::Result<()> {
        let Some(path) = snapshot.path.as_deref() else {
            return Ok(());
        };
        let mut written = self.written.lock().unwrap_or_else(PoisonError::into_inner);
        if snapshot.generation <= *written {
            return Ok(());
        }
        let devices: Vec<String> = snapshot.hashes.iter().map(|hash| hex(hash)).collect();
        let text = json!({ "version": FILE_VERSION, "devices": devices }).to_string();
        write_replacing(path, text.as_bytes())?;
        *written = snapshot.generation;
        Ok(())
    }
}

impl TokenStore {
    /// The store at `path`; a missing file is an empty store, an unreadable
    /// or damaged one too, reported so the host can log it.
    pub fn open(path: Option<PathBuf>) -> (TokenStore, Option<LoadProblem>) {
        let mut store = TokenStore {
            path,
            hashes: Vec::new(),
            generation: 0,
        };
        let Some(path) = store.path.as_deref() else {
            return (store, None);
        };
        let problem = match fs::read(path) {
            Ok(bytes) => match parse_file(&bytes) {
                Some(hashes) => {
                    store.hashes = hashes;
                    None
                }
                None => Some(LoadProblem::NotInFormat),
            },
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => Some(LoadProblem::Unreadable(error)),
        };
        (store, problem)
    }

    pub fn len(&self) -> usize {
        self.hashes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    /// Whether `token` belongs to a paired device.
    pub fn contains(&self, token: &str) -> bool {
        if token.len() != 32 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return false;
        }
        let hash = token_hash(token);
        self.hashes.iter().fold(false, |found, kept| {
            found | equal_in_constant_time(kept, &hash)
        })
    }

    /// Remembers a new device's token, dropping the oldest beyond
    /// [`MAX_DEVICES`]. The token works at once, saved or not, until the
    /// app quits.
    pub fn add(&mut self, token: &str) -> Snapshot {
        self.hashes.push(token_hash(token));
        if self.hashes.len() > MAX_DEVICES {
            let excess = self.hashes.len() - MAX_DEVICES;
            self.hashes.drain(..excess);
        }
        self.snapshot()
    }

    /// Forgets one device; none when it wasn't paired.
    pub fn remove(&mut self, token: &str) -> Option<Snapshot> {
        let hash = token_hash(token);
        let before = self.hashes.len();
        self.hashes.retain(|kept| *kept != hash);
        (self.hashes.len() != before).then(|| self.snapshot())
    }

    /// Forgets every device.
    pub fn clear(&mut self) -> Snapshot {
        self.hashes.clear();
        self.snapshot()
    }

    fn snapshot(&mut self) -> Snapshot {
        self.generation += 1;
        Snapshot {
            path: self.path.clone(),
            hashes: self.hashes.clone(),
            generation: self.generation,
        }
    }
}

fn parse_file(bytes: &[u8]) -> Option<Vec<TokenHash>> {
    let value: Value = serde_json::from_slice(bytes).ok()?;
    if value.get("version")?.as_u64()? != FILE_VERSION {
        return None;
    }
    let mut hashes: Vec<TokenHash> = value
        .get("devices")?
        .as_array()?
        .iter()
        .map(|item| item.as_str().and_then(from_hex))
        .collect::<Option<_>>()?;
    if hashes.len() > MAX_DEVICES {
        hashes.drain(..hashes.len() - MAX_DEVICES);
    }
    Some(hashes)
}

/// Writes next to `path` and renames over it, so a crash leaves the old
/// file or the new one, never half of one.
fn write_replacing(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".new");
    let temporary = PathBuf::from(temporary);
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn peer(last: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(127, 0, 0, last))
    }

    fn short() -> Limits {
        Limits {
            code_lifetime: Duration::from_secs(600),
            window: Duration::from_secs(60),
        }
    }

    fn wrong_for(code: &str) -> String {
        let digit = (code.as_bytes()[0] - b'0' + 1) % 10;
        format!("{digit}{}", &code[1..])
    }

    #[test]
    fn codes_have_six_digits() {
        for _ in 0..200 {
            let code = random_code().unwrap();
            assert_eq!(code.len(), CODE_DIGITS);
            assert!(code.bytes().all(|byte| byte.is_ascii_digit()));
        }
    }

    #[test]
    fn the_right_code_pairs_once() {
        let now = Instant::now();
        let mut pairing = Pairing::new(short(), now).unwrap();
        let code = pairing.current(now).unwrap().to_owned();
        assert_eq!(pairing.attempt(peer(1), &code, now), PairOutcome::Paired);
        assert_eq!(pairing.attempt(peer(1), &code, now), PairOutcome::Wrong);
        let next = pairing.current(now).unwrap().to_owned();
        assert_ne!(next, code);
        assert_eq!(pairing.attempt(peer(1), &next, now), PairOutcome::Paired);
    }

    #[test]
    fn a_code_expires() {
        let start = Instant::now();
        let mut pairing = Pairing::new(short(), start).unwrap();
        let code = pairing.current(start).unwrap().to_owned();
        let later = start + Duration::from_secs(600);
        assert_eq!(pairing.current(later), None);
        assert_eq!(pairing.attempt(peer(1), &code, later), PairOutcome::Expired);
        let fresh = pairing.rotate(later).unwrap();
        assert_eq!(pairing.attempt(peer(1), &fresh, later), PairOutcome::Paired);
    }

    #[test]
    fn five_wrong_codes_a_minute_per_address_then_429() {
        let start = Instant::now();
        let mut pairing = Pairing::new(short(), start).unwrap();
        let code = pairing.current(start).unwrap().to_owned();
        let wrong = wrong_for(&code);
        for _ in 0..WRONG_PER_WINDOW {
            assert_eq!(pairing.attempt(peer(1), &wrong, start), PairOutcome::Wrong);
        }
        assert_eq!(pairing.attempt(peer(1), &code, start), PairOutcome::Limited);
        // Another address is counted on its own.
        assert_eq!(pairing.attempt(peer(2), &wrong, start), PairOutcome::Wrong);
        let next_minute = start + Duration::from_secs(60);
        assert_eq!(
            pairing.attempt(peer(1), &code, next_minute),
            PairOutcome::Paired
        );
    }

    #[test]
    fn twenty_wrong_codes_lock_pairing_until_rotation() {
        let start = Instant::now();
        let mut pairing = Pairing::new(short(), start).unwrap();
        let code = pairing.current(start).unwrap().to_owned();
        let wrong = wrong_for(&code);
        for index in 0..WRONG_BEFORE_LOCK {
            let from = peer(1 + (index as usize / WRONG_PER_WINDOW) as u8);
            assert_eq!(pairing.attempt(from, &wrong, start), PairOutcome::Wrong);
        }
        assert!(pairing.locked());
        assert_eq!(pairing.attempt(peer(50), &code, start), PairOutcome::Locked);
        let later = start + Duration::from_secs(300);
        assert_eq!(pairing.attempt(peer(50), &code, later), PairOutcome::Locked);
        let fresh = pairing.rotate(later).unwrap();
        assert!(!pairing.locked());
        assert_eq!(pairing.attempt(peer(1), &fresh, later), PairOutcome::Paired);
    }

    #[test]
    fn tries_refused_as_limited_never_count_toward_the_lock() {
        // Otherwise one address could lock pairing for every phone within a minute.
        let start = Instant::now();
        let mut pairing = Pairing::new(short(), start).unwrap();
        let code = pairing.current(start).unwrap().to_owned();
        let wrong = wrong_for(&code);
        for _ in 0..WRONG_PER_WINDOW {
            assert_eq!(pairing.attempt(peer(1), &wrong, start), PairOutcome::Wrong);
        }
        for _ in 0..WRONG_BEFORE_LOCK * 2 {
            assert_eq!(
                pairing.attempt(peer(1), &wrong, start),
                PairOutcome::Limited
            );
        }
        assert!(!pairing.locked());
        assert_eq!(pairing.attempt(peer(2), &code, start), PairOutcome::Paired);
    }

    #[test]
    fn rotating_retires_the_old_code_and_clears_the_limit() {
        let start = Instant::now();
        let mut pairing = Pairing::new(short(), start).unwrap();
        let old = pairing.current(start).unwrap().to_owned();
        let wrong = wrong_for(&old);
        for _ in 0..WRONG_PER_WINDOW {
            pairing.attempt(peer(1), &wrong, start);
        }
        assert_eq!(pairing.attempt(peer(1), &old, start), PairOutcome::Limited);
        let fresh = pairing.rotate(start).unwrap();
        assert_eq!(pairing.current(start), Some(fresh.as_str()));
        if fresh != old {
            assert_eq!(pairing.attempt(peer(1), &old, start), PairOutcome::Wrong);
        }
        assert_eq!(pairing.attempt(peer(1), &fresh, start), PairOutcome::Paired);
    }

    #[test]
    fn a_code_is_matched_whole_after_trimming() {
        let start = Instant::now();
        let mut pairing = Pairing::new(short(), start).unwrap();
        let code = pairing.current(start).unwrap().to_owned();
        let near_misses = [
            String::new(),
            code[..5].to_owned(),
            format!("{code}0"),
            format!("0{code}"),
            format!("{} {}", &code[..3], &code[3..]),
        ];
        for (index, near) in near_misses.iter().enumerate() {
            assert_eq!(
                pairing.attempt(peer(10 + index as u8), near, start),
                PairOutcome::Wrong,
                "{near:?}"
            );
        }
        assert_eq!(
            pairing.attempt(peer(1), &format!(" \t{code}\n"), start),
            PairOutcome::Paired
        );
    }

    #[test]
    fn the_wrong_code_window_slides_with_each_try() {
        let start = Instant::now();
        let mut pairing = Pairing::new(short(), start).unwrap();
        let code = pairing.current(start).unwrap().to_owned();
        let wrong = wrong_for(&code);
        for second in 0..WRONG_PER_WINDOW as u64 {
            let at = start + Duration::from_secs(second * 10);
            assert_eq!(pairing.attempt(peer(1), &wrong, at), PairOutcome::Wrong);
        }
        let almost = start + Duration::from_secs(60) - Duration::from_millis(1);
        assert_eq!(
            pairing.attempt(peer(1), &code, almost),
            PairOutcome::Limited
        );
        // The first wrong try leaves the window, so exactly one more is let through.
        let first_gone = start + Duration::from_secs(60);
        assert_eq!(
            pairing.attempt(peer(1), &wrong, first_gone),
            PairOutcome::Wrong
        );
        assert_eq!(
            pairing.attempt(peer(1), &code, first_gone),
            PairOutcome::Limited
        );
        let second_gone = start + Duration::from_secs(70);
        assert_eq!(
            pairing.attempt(peer(1), &code, second_gone),
            PairOutcome::Paired
        );
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("test-scratch")
            .join(name);
        let _ = fs::remove_dir_all(&dir);
        dir.join("remote-devices.json")
    }

    #[test]
    fn tokens_are_kept_as_hashes_eight_at_most() {
        let path = scratch("auth-tokens");
        let (mut store, problem) = TokenStore::open(Some(path.clone()));
        assert!(problem.is_none());
        let device_file = DeviceFile::default();
        let tokens: Vec<String> = (0..MAX_DEVICES + 1)
            .map(|_| random_token().unwrap())
            .collect();
        for token in &tokens {
            device_file.save(store.add(token)).unwrap();
        }
        assert_eq!(store.len(), MAX_DEVICES);
        assert!(!store.contains(&tokens[0]), "the oldest is dropped");
        assert!(tokens[1..].iter().all(|token| store.contains(token)));

        let file = fs::read_to_string(&path).unwrap();
        assert!(tokens.iter().all(|token| !file.contains(token.as_str())));
        assert!(file.contains(&hex(&token_hash(&tokens[1]))));

        let (reopened, problem) = TokenStore::open(Some(path.clone()));
        assert!(problem.is_none());
        assert_eq!(reopened.len(), MAX_DEVICES);
        assert!(reopened.contains(&tokens[MAX_DEVICES]));

        device_file.save(store.remove(&tokens[3]).unwrap()).unwrap();
        assert!(store.remove(&tokens[3]).is_none());
        assert!(!TokenStore::open(Some(path.clone())).0.contains(&tokens[3]));
        device_file.save(store.clear()).unwrap();
        assert!(TokenStore::open(Some(path)).0.is_empty());
    }

    #[test]
    fn a_snapshot_saved_late_never_takes_the_file_back() {
        let path = scratch("auth-order");
        let (mut store, _) = TokenStore::open(Some(path.clone()));
        let device_file = DeviceFile::default();
        let first = random_token().unwrap();
        let second = random_token().unwrap();
        let older = store.add(&first);
        let newer = store.add(&second);
        device_file.save(newer).unwrap();
        device_file.save(older).unwrap();
        assert!(TokenStore::open(Some(path)).0.contains(&second));
    }

    #[test]
    fn a_damaged_file_starts_empty() {
        let path = scratch("auth-damaged");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{\"version\": 1, \"devices\": [\"zz\"]}").unwrap();
        let (store, problem) = TokenStore::open(Some(path));
        assert!(store.is_empty());
        assert!(matches!(problem, Some(LoadProblem::NotInFormat)));
    }

    #[test]
    fn only_well_formed_tokens_are_looked_up() {
        let (mut store, _) = TokenStore::open(None);
        let token = random_token().unwrap();
        DeviceFile::default().save(store.add(&token)).unwrap();
        assert!(store.contains(&token));
        assert!(!store.contains(""));
        assert!(!store.contains(&token[..31]));
        assert!(!store.contains(&format!("{}g", &token[..31])));
    }
}
