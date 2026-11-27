//! The lines `fake_tv` reads on stdin, in their own file so a test includes it.

pub const COMMANDS: &str =
    "vol <dB>, loss <percent>, loss off, hide, show, sleep, sleep quiet, wake, quit";

#[derive(Debug, PartialEq)]
pub enum Line {
    Vol(f64),
    BadVol,
    Loss(f64),
    BadLoss,
    Hide,
    Show,
    /// Asleep off the network; false for `sleep quiet`, without the goodbye.
    Sleep {
        goodbye: bool,
    },
    Wake,
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
        (Some("sleep"), None) => Line::Sleep { goodbye: true },
        (Some("sleep"), Some("quiet")) => Line::Sleep { goodbye: false },
        (Some("wake"), _) => Line::Wake,
        (Some("vol"), Some(db)) => db.parse().map_or(Line::BadVol, Line::Vol),
        (Some("loss"), Some("off")) => Line::Loss(0.0),
        (Some("loss"), Some(percent)) => percent
            .parse()
            .ok()
            .filter(|percent| (0.0..=100.0).contains(percent))
            .map_or(Line::BadLoss, Line::Loss),
        (None, _) => Line::Empty,
        _ => Line::Unknown,
    }
}
