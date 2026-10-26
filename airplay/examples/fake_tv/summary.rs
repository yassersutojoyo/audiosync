//! The plain lines `fake_tv --summary` prints: one per event a dry-run sheet
//! judges (pairing, SETUP, RECORD, FLUSH, TEARDOWN, volume either way), each
//! with its running count, so a cell reads as PASS or FAIL without the raw
//! request log. Shared with `tests/fake_tv_summary.rs`.

use std::collections::HashMap;

use audiosync_airplay::rtsp::{Method, Request};

use crate::common::ap2::request_key;
use crate::common::hap::{item, untlv};

/// The first volume set from the app after `vol`, this close to the level
/// `vol` set, is the app sending the television's own change back.
pub const ECHO_TOLERANCE_DB: f64 = 1.0;

pub fn stamp(seconds: f64) -> String {
    format!("[{seconds:9.3}]")
}

/// The level in a `volume: <dB>` text, as volume sets carry it and reads
/// answer it.
pub fn parse_level(text: &str) -> Option<f64> {
    text.trim().strip_prefix("volume:")?.trim().parse().ok()
}

#[derive(Default)]
pub struct Summary {
    counts: HashMap<&'static str, usize>,
    /// When `vol` last moved the level and to what, while no volume set
    /// from the app has come since.
    unanswered: Option<(f64, f64)>,
    /// The level `vol` set, until the app has read it once.
    unread: Option<f64>,
}

impl Summary {
    pub fn connection(&mut self, at: f64) -> String {
        self.line(at, "connection", " opened")
    }

    /// `vol <dB>` typed on the fake; `db` is the level after the remote's grid.
    pub fn pushed(&mut self, at: f64, db: f64) -> String {
        self.unanswered = Some((at, db));
        self.unread = Some(db);
        self.line(at, "volume set with vol", &format!(": {db:.1} dB"))
    }

    /// One answered request with the level a read answered right after it;
    /// `None` for the kinds no cell judges.
    pub fn request(
        &mut self,
        at: f64,
        request: &Request,
        status: u16,
        level: Option<&str>,
    ) -> Option<String> {
        // The television answers every volume set with 500 and still applies
        // it, so only the other kinds report a refusal.
        let refused = if status >= 300 {
            format!(" - refused with status {status}")
        } else {
            String::new()
        };
        let line = match request.method {
            Method::Post if request.uri == "/pair-setup" => {
                let items = untlv(&request.body);
                let transient = item(&items, 19).is_some();
                match item(&items, 6).and_then(|state| state.first().copied()) {
                    Some(1) if transient => self.line(at, "pairing without PIN", &refused),
                    Some(1) => self.line(at, "PIN pairing started", &refused),
                    // The sender sends M5 only after the PIN proof was accepted.
                    Some(5) => self.line(at, "PIN pairing done", &refused),
                    _ => return None,
                }
            }
            Method::Setup if request_key(request) == "SETUP stream" => {
                self.line(at, "SETUP stream", &refused)
            }
            Method::Setup => self.line(at, "SETUP session", &refused),
            Method::Record => self.line(at, "RECORD", &format!(" (play starts){refused}")),
            Method::Flush => self.line(at, "FLUSH", &format!(" (pause){refused}")),
            Method::Teardown => self.line(at, "TEARDOWN", &format!(" (stream ends){refused}")),
            Method::SetParameter => return self.volume_set(at, &request.body),
            Method::GetParameter if request.body.starts_with(b"volume") => {
                let unread = self.unread?;
                // A read answered just before `vol` still carries the old level.
                if level.and_then(parse_level) != Some(unread) {
                    return None;
                }
                self.unread = None;
                self.line(at, "app read the new level", &format!(": {unread:.1} dB"))
            }
            _ => return None,
        };
        Some(line)
    }

    fn volume_set(&mut self, at: f64, body: &[u8]) -> Option<String> {
        let db = parse_level(std::str::from_utf8(body).ok()?)?;
        Some(match self.unanswered.take() {
            Some((pushed_at, tv)) if (db - tv).abs() <= ECHO_TOLERANCE_DB => self.line(
                at,
                "ECHO",
                &format!(
                    ": the app sent {db:.1} dB back {:.1} s after vol",
                    at - pushed_at
                ),
            ),
            Some((_, tv)) => self.line(
                at,
                "OVERWRITE",
                &format!(": app sent {db:.1} dB, TV is at {tv:.1} dB"),
            ),
            None => self.line(at, "volume from the app", &format!(": {db:.1} dB")),
        })
    }

    fn line(&mut self, at: f64, label: &'static str, detail: &str) -> String {
        let count = self.counts.entry(label).or_default();
        *count += 1;
        format!("{} {label} #{count}{detail}", stamp(at))
    }
}
