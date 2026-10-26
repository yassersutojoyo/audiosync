//! Dotted version numbers such as `0.7.10` or `10.0.22000` (spec S296).

use std::cmp::Ordering;
use std::fmt;

/// A version as decimal parts; a missing part counts as 0 when compared, so
/// `0.8` equals `0.8.0`.
#[derive(Clone, Debug, Eq)]
pub struct Version(Vec<u64>);

impl Version {
    /// `None` when the text is empty or any dot-separated part isn't a
    /// decimal number.
    pub fn parse(text: &str) -> Option<Version> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        text.split('.')
            .map(|part| {
                if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                    None
                } else {
                    part.parse().ok()
                }
            })
            .collect::<Option<Vec<u64>>>()
            .map(Version)
    }

    pub fn from_parts(parts: &[u64]) -> Version {
        Version(parts.to_vec())
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Version) -> Ordering {
        let len = self.0.len().max(other.0.len());
        (0..len)
            .map(|i| {
                let a = self.0.get(i).copied().unwrap_or(0);
                let b = other.0.get(i).copied().unwrap_or(0);
                a.cmp(&b)
            })
            .find(|order| order.is_ne())
            .unwrap_or(Ordering::Equal)
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Version) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Version) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self.0.iter().map(u64::to_string).collect();
        f.write_str(&parts.join("."))
    }
}
