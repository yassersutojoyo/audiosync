//! The lines `fake_tv` reads on stdin, in their own file so a test includes it.

pub const COMMANDS: &str = "vol <dB>, hide, show, quit";

#[derive(Debug, PartialEq)]
pub enum Line {
    Vol(f64),
    BadVol,
    Hide,
    Show,
    Quit,
    Empty,
    Unknown,
}

pub fn read(line: &str) -> Line {
    let mut words = line.split_whitespace();
    match (words.next(), words.next()) {
        (Some("quit"), _) => Line::Quit,
        (Some("hide"), _) => Line::Hide,
        (Some("show"), _) => Line::Show,
        (Some("vol"), Some(db)) => db.parse().map_or(Line::BadVol, Line::Vol),
        (None, _) => Line::Empty,
        _ => Line::Unknown,
    }
}
