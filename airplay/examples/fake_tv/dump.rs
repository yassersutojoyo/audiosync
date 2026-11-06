//! `--dump <dir>`: every request the fake answered, decrypted, one numbered
//! file each with its headers, its body and the answer's body, plus the
//! lines a latency measurement reads from them.

#![allow(dead_code)]

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use audiosync_airplay::rtsp::Request;
use plist::Value;

use crate::common::ap2::{Answered, OTHER_METHOD, read_dict, read_value, request_key};

pub struct Dump {
    dir: PathBuf,
    written: usize,
    /// The timing protocol and peers of the last session SETUP or SETPEERS.
    timing: String,
}

impl Dump {
    pub fn create(dir: &Path) -> io::Result<Dump> {
        fs::create_dir_all(dir)?;
        Ok(Dump {
            dir: dir.to_owned(),
            written: 0,
            timing: "timing -".to_owned(),
        })
    }

    /// Writes the next numbered file and gives back the lines it adds to
    /// stdout.
    pub fn record(&mut self, request: &Request, answered: &Answered) -> io::Result<Vec<String>> {
        let key = request_key(request);
        self.written += 1;
        let method = request
            .headers
            .get(OTHER_METHOD)
            .unwrap_or(request.method.as_str());
        let mut text = format!("{method} {}\n", request.uri);
        for (name, value) in request.headers.iter() {
            if name != OTHER_METHOD {
                let _ = writeln!(text, "{name}: {value}");
            }
        }
        let _ = writeln!(text, "\n{}", render(&request.body));
        let _ = writeln!(
            text,
            "--- answer {} at unix {:.6}\n{}",
            answered.status,
            unix(answered.at),
            render(&answered.body)
        );
        let path = self
            .dir
            .join(format!("{:04}-{}.txt", self.written, label(&key)));
        fs::write(path, text)?;
        Ok(self.lines(&key, request, answered))
    }

    fn lines(&mut self, key: &str, request: &Request, answered: &Answered) -> Vec<String> {
        let body = read_dict(&request.body).unwrap_or_default();
        match key {
            "SETUP session" => {
                let peers = body
                    .get("timingPeerInfo")
                    .or_else(|| body.get("timingPeerList"));
                self.timing = format!(
                    "timing {} peers {}",
                    field(body.get("timingProtocol")),
                    field(peers)
                );
                vec![format!("dump: session SETUP, {}", self.timing)]
            }
            "SETUP stream" => {
                let streams = body.get("streams").and_then(Value::as_array);
                streams
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_dictionary)
                    .map(|stream| {
                        let get = |name: &str| field(stream.get(name).or_else(|| body.get(name)));
                        let kind = match stream.get("type").and_then(Value::as_signed_integer) {
                            Some(96) => "96 realtime",
                            Some(103) => "103 buffered",
                            _ => "other",
                        };
                        format!(
                            "dump: stream SETUP type {kind}, latencyMin {} latencyMax {} audioMode {} ct {} spf {} sr {} supportsDynamicStreamID {}, {}",
                            get("latencyMin"),
                            get("latencyMax"),
                            get("audioMode"),
                            get("ct"),
                            get("spf"),
                            get("sr"),
                            get("supportsDynamicStreamID"),
                            self.timing
                        )
                    })
                    .collect()
            }
            "SETPEERS" | "SETPEERSX" => {
                let peers = read_value(&request.body);
                self.timing = format!("timing peers {}", field(peers.as_ref()));
                vec![format!("dump: {key} {}", field(peers.as_ref()))]
            }
            "SETRATEANCHORTIME" => vec![format!(
                "dump: anchor rtpTime {} networkTimeSecs {} networkTimeFrac {} networkTimeTimelineID {} rate {}, received at unix {:.6}",
                field(body.get("rtpTime")),
                field(body.get("networkTimeSecs")),
                field(body.get("networkTimeFrac")),
                field(body.get("networkTimeTimelineID")),
                field(body.get("rate")),
                unix(answered.at)
            )],
            _ => Vec::new(),
        }
    }
}

pub fn unix(at: SystemTime) -> f64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0.0, |since| since.as_secs_f64())
}

/// A key as a file name part: `GET /info` becomes `GET-info`.
fn label(key: &str) -> String {
    key.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// A binary plist as XML, text as it is, anything else as hex.
fn render(body: &[u8]) -> String {
    if body.is_empty() {
        return "(no body)".to_owned();
    }
    if body.starts_with(b"bplist")
        && let Some(value) = read_value(body)
    {
        let mut xml = Vec::new();
        if value.to_writer_xml(&mut xml).is_ok() {
            return String::from_utf8_lossy(&xml).into_owned();
        }
    }
    if let Ok(text) = std::str::from_utf8(body)
        && !text.chars().any(|c| c.is_control() && !c.is_whitespace())
    {
        return text.to_owned();
    }
    let hex: String = body.iter().map(|byte| format!("{byte:02x}")).collect();
    format!("({} bytes) {hex}", body.len())
}

/// One plist value on one line, `-` when absent.
fn field(value: Option<&Value>) -> String {
    let Some(value) = value else {
        return "-".to_owned();
    };
    match value {
        Value::String(text) => text.clone(),
        Value::Integer(number) => number.to_string(),
        Value::Real(number) => number.to_string(),
        Value::Boolean(flag) => flag.to_string(),
        Value::Data(bytes) => format!("<{} bytes>", bytes.len()),
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(|item| field(Some(item))).collect();
            format!("[{}]", items.join(", "))
        }
        Value::Dictionary(entries) => {
            let entries: Vec<String> = entries
                .iter()
                .map(|(name, item)| format!("{name}: {}", field(Some(item))))
                .collect();
            format!("{{{}}}", entries.join(", "))
        }
        other => format!("{other:?}"),
    }
}
