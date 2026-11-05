//! The per-device key pins of device authentication (C129 to C133): what
//! each challenge outcome does to a device's entry, the log line it writes,
//! and the file the entries are kept in across launches.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use crate::auth::{AUTH_WAIT, KeyPin, MARKED_AUTH_WAIT, Outcome};

/// What the host keeps for one device (C129).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    Pin(KeyPin),
    /// The device did not answer the challenge or declined it.
    Unauthenticated,
}

/// What a challenge's outcome does to the connect (C129, C130).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Proceed,
    /// The connect fails and is retried (C80, C81).
    ConnectFailed,
    /// Not retried; the host turns the target off (C130).
    Refused,
}

/// A judged challenge: the verdict, whether the host shows the notice of
/// C131, and the debug line of C132.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub verdict: Verdict,
    pub warn: bool,
    pub line: String,
}

/// How long the challenge waits for a device with `entry` (C119).
pub fn auth_wait(entry: Option<Entry>) -> std::time::Duration {
    match entry {
        Some(Entry::Unauthenticated) => MARKED_AUTH_WAIT,
        _ => AUTH_WAIT,
    }
}

/// What `outcome` does for a device with `entry` (C129), and the entry it
/// leaves: `None` keeps the one there.
pub fn decide(
    key: &str,
    entry: Option<Entry>,
    outcome: &Outcome,
    group: bool,
) -> (Decision, Option<Entry>) {
    let decision = |verdict, warn, text: String| Decision {
        verdict,
        warn,
        line: format!("cast auth {key}: {text}"),
    };
    match outcome {
        Outcome::Verified(verified) => {
            let text = outcome.to_string();
            match entry {
                _ if group => (decision(Verdict::Proceed, false, text), None),
                Some(Entry::Pin(pin)) if pin == verified.key => {
                    (decision(Verdict::Proceed, false, text), None)
                }
                Some(Entry::Pin(pin)) => (
                    decision(
                        Verdict::Proceed,
                        false,
                        format!(
                            "re-pinned, key changed from {} to {}",
                            pin.short(),
                            verified.key.short()
                        ),
                    ),
                    Some(Entry::Pin(verified.key)),
                ),
                None | Some(Entry::Unauthenticated) => (
                    decision(
                        Verdict::Proceed,
                        false,
                        format!("pinned {}", verified.key.short()),
                    ),
                    Some(Entry::Pin(verified.key)),
                ),
            }
        }
        Outcome::Failed(_) => (
            decision(Verdict::Refused, false, format!("{outcome}, refused")),
            None,
        ),
        Outcome::Declined(_) | Outcome::NoAnswer(_) => match entry {
            Some(Entry::Pin(_)) if !group => (
                decision(
                    Verdict::ConnectFailed,
                    false,
                    format!("{outcome}, connect failed"),
                ),
                None,
            ),
            None => (
                decision(
                    Verdict::Proceed,
                    true,
                    format!("{outcome}, unauthenticated, warned"),
                ),
                Some(Entry::Unauthenticated),
            ),
            Some(_) => (
                decision(
                    Verdict::Proceed,
                    false,
                    format!("{outcome}, unauthenticated"),
                ),
                None,
            ),
        },
    }
}

/// How the host replaces the file with new bytes.
type BookWriter = Box<dyn FnMut(&Path, &[u8]) -> io::Result<()> + Send>;
/// What hears the first failed write.
type SaveErrorReport = Box<dyn FnOnce(io::Error) + Send>;

/// The entries of every device, kept in `cast-device-keys.txt` (C129).
#[derive(Default)]
pub struct DeviceKeyBook {
    entries: HashMap<String, Entry>,
    file: Option<(PathBuf, BookWriter)>,
    report_save_error: Option<SaveErrorReport>,
}

impl fmt::Debug for DeviceKeyBook {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeviceKeyBook")
            .field("entries", &self.entries)
            .finish_non_exhaustive()
    }
}

impl DeviceKeyBook {
    pub fn new() -> DeviceKeyBook {
        DeviceKeyBook::default()
    }

    /// The book kept at `path`, read through `read` and rewritten whole
    /// through `write` at each change. A file that is there but can't be
    /// read starts an empty book, and a line that doesn't read is left out;
    /// either comes with the error for the host's log.
    pub fn open(
        path: PathBuf,
        read: impl FnOnce(&Path) -> io::Result<Vec<u8>>,
        write: impl FnMut(&Path, &[u8]) -> io::Result<()> + Send + 'static,
    ) -> (DeviceKeyBook, Option<io::Error>) {
        let text = read(&path).and_then(|bytes| {
            String::from_utf8(bytes)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
        });
        let (entries, error) = match text {
            Ok(text) => parse_book(&text),
            Err(error) if error.kind() == io::ErrorKind::NotFound => (HashMap::new(), None),
            Err(error) => (HashMap::new(), Some(error)),
        };
        let book = DeviceKeyBook {
            entries,
            file: Some((path, Box::new(write))),
            report_save_error: None,
        };
        (book, error)
    }

    /// Hands the first write that fails to `report`; later ones go
    /// unreported, so a file that stays locked logs once.
    pub fn on_save_error(mut self, report: impl FnOnce(io::Error) + Send + 'static) -> Self {
        self.report_save_error = Some(Box::new(report));
        self
    }

    pub fn entry(&self, key: &str) -> Option<Entry> {
        self.entries.get(key).copied()
    }

    /// Applies `outcome` to `key`'s entry (C129) and saves a change.
    pub fn judge(&mut self, key: &str, outcome: &Outcome, group: bool) -> Decision {
        let (decision, entry) = decide(key, self.entry(key), outcome, group);
        if let Some(entry) = entry {
            self.entries.insert(key.to_owned(), entry);
            self.save();
        }
        decision
    }

    /// Clears `key`'s entry when the user removes the device (C133); the
    /// line to log when there was one.
    pub fn clear(&mut self, key: &str) -> Option<String> {
        let entry = self.entries.remove(key)?;
        self.save();
        Some(match entry {
            Entry::Pin(_) => format!("cast auth {key}: pin cleared"),
            Entry::Unauthenticated => format!("cast auth {key}: mark cleared"),
        })
    }

    fn save(&mut self) {
        let Some((path, write)) = &mut self.file else {
            return;
        };
        let mut keys: Vec<&String> = self.entries.keys().collect();
        keys.sort();
        let text: String = (keys.into_iter())
            .map(|key| match self.entries[key] {
                Entry::Pin(pin) => format!("{key} {pin}\n"),
                Entry::Unauthenticated => format!("{key} unauthenticated\n"),
            })
            .collect();
        // A book that can't be written stays in memory for this launch.
        if let Err(error) = write(path, text.as_bytes())
            && let Some(report) = self.report_save_error.take()
        {
            report(error);
        }
    }
}

/// The entries of `text`, and the error of its first line that doesn't read.
fn parse_book(text: &str) -> (HashMap<String, Entry>, Option<io::Error>) {
    let mut error = None;
    // `lines` also takes CRLF endings, as an editor on Windows may save.
    let entries = (text.lines())
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let (key, value) = line.split_once(' ').unwrap_or((line, ""));
            let entry = match value {
                "unauthenticated" => Some(Entry::Unauthenticated),
                hex => pin_from_hex(hex).map(Entry::Pin),
            };
            match entry {
                Some(entry) if !key.is_empty() => Ok((key.to_owned(), entry)),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "not a device and a key: {:?}",
                        line.chars().take(80).collect::<String>()
                    ),
                )),
            }
            .map_err(|bad| error = error.take().or(Some(bad)))
            .ok()
        })
        .collect();
    (entries, error)
}

fn pin_from_hex(hex: &str) -> Option<KeyPin> {
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mut pin = [0u8; 32];
    for (index, byte) in pin.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(KeyPin(pin))
}
