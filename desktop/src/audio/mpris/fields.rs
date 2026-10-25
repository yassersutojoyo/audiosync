//! What an MPRIS player's `Player` properties say, read from a plain copy of
//! their values ([`Field`]), so the rules hold and are tested on every
//! system. A missing or wrong-typed value counts as absent, never as an
//! error.

// Outside Linux only the tests read players.
#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use std::collections::HashMap;
use std::path::PathBuf;

use crate::audio::now_playing::{MediaSession, RawTimeline};
use crate::now_playing_card::Transport;

/// A property's value, as far as the mapping reads it.
#[derive(Clone, Debug, PartialEq)]
pub enum Field {
    Text(String),
    Texts(Vec<String>),
    /// Any integer type the player sent, when it fits.
    Integer(i64),
    Bool(bool),
    Map(HashMap<String, Field>),
    /// A type the mapping never reads.
    Other,
}

/// A player that has a current track, as one listing read it.
#[derive(Clone, Debug, PartialEq)]
pub struct Player {
    pub session: MediaSession,
    pub art_url: Option<String>,
    /// `None` without a length or a position.
    pub timeline: Option<RawTimeline>,
    pub controls: Controls,
}

/// Which of the card's buttons the player takes now.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Controls {
    pub previous: bool,
    pub play_pause: bool,
    pub next: bool,
}

impl Controls {
    pub fn allows(self, command: Transport) -> bool {
        match command {
            Transport::Previous => self.previous,
            Transport::PlayPause => self.play_pause,
            Transport::Next => self.next,
        }
    }
}

/// The player on `bus_name` from its `Player` properties; `None` when it has
/// no current track: stopped, or a status that is neither playing nor
/// paused.
pub fn player(bus_name: &str, properties: &HashMap<String, Field>) -> Option<Player> {
    let playing = match text(properties.get("PlaybackStatus"))? {
        "Playing" => true,
        "Paused" => false,
        _ => return None,
    };
    let metadata = match properties.get("Metadata") {
        Some(Field::Map(metadata)) => Some(metadata),
        _ => None,
    };
    let meta = |key: &str| metadata.and_then(|metadata| metadata.get(key));
    let owned = |key: &str| text(meta(key)).unwrap_or_default().to_owned();
    let artist = match meta("xesam:artist") {
        Some(Field::Texts(names)) => names.join(", "),
        // Some players send one name where the specification has a list.
        Some(Field::Text(name)) => name.clone(),
        _ => String::new(),
    };
    let length = integer(meta("mpris:length")).filter(|length| *length > 0);
    let position = integer(properties.get("Position"));
    let timeline = length.zip(position).map(|(length, position)| RawTimeline {
        start: 0,
        end: ticks(length),
        position: ticks(position.max(0)),
        updated: 0,
    });
    let flag = |key: &str| matches!(properties.get(key), Some(Field::Bool(true)));
    Some(Player {
        session: MediaSession {
            app_id: bus_name.to_owned(),
            title: owned("xesam:title"),
            artist,
            album: owned("xesam:album"),
            playing,
            paused: !playing,
        },
        art_url: text(meta("mpris:artUrl")).map(str::to_owned),
        timeline,
        controls: Controls {
            previous: flag("CanGoPrevious"),
            // PlayPause fails on a player that can't pause.
            play_pause: flag("CanPause"),
            next: flag("CanGoNext"),
        },
    })
}

fn text(field: Option<&Field>) -> Option<&str> {
    match field {
        Some(Field::Text(text)) => Some(text),
        _ => None,
    }
}

fn integer(field: Option<&Field>) -> Option<i64> {
    match field {
        Some(Field::Integer(value)) => Some(*value),
        _ => None,
    }
}

/// Microseconds in the 100 ns units of [`RawTimeline`].
fn ticks(microseconds: i64) -> i64 {
    microseconds.saturating_mul(10)
}

/// The local file a `file://` URL names; `None` for any other scheme, a
/// remote host, or a path that isn't UTF-8 once decoded.
pub fn art_path(url: &str) -> Option<PathBuf> {
    let rest = url.strip_prefix("file://")?;
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    if !rest.starts_with('/') {
        return None;
    }
    let path = rest.split(['?', '#']).next().unwrap_or(rest);
    let mut bytes = Vec::with_capacity(path.len());
    let mut iter = path.bytes();
    while let Some(byte) = iter.next() {
        if byte == b'%' {
            let high = iter.next().and_then(hex)?;
            let low = iter.next().and_then(hex)?;
            bytes.push((high << 4) | low);
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).ok().map(PathBuf::from)
}

fn hex(digit: u8) -> Option<u8> {
    char::from(digit)
        .to_digit(16)
        .and_then(|value| u8::try_from(value).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn properties(entries: Vec<(&str, Field)>) -> HashMap<String, Field> {
        entries
            .into_iter()
            .map(|(key, field)| (key.to_owned(), field))
            .collect()
    }

    fn text(value: &str) -> Field {
        Field::Text(value.to_owned())
    }

    fn song(status: &str, metadata: Vec<(&str, Field)>) -> HashMap<String, Field> {
        properties(vec![
            ("PlaybackStatus", text(status)),
            ("Metadata", Field::Map(properties(metadata))),
            ("Position", Field::Integer(50_000_000)),
            ("CanGoPrevious", Field::Bool(true)),
            ("CanPause", Field::Bool(true)),
            ("CanGoNext", Field::Bool(false)),
        ])
    }

    const NAME: &str = "org.mpris.MediaPlayer2.vlc";

    #[test]
    fn a_playing_players_track_is_read_with_its_artists_joined() {
        let read = player(
            NAME,
            &song(
                "Playing",
                vec![
                    ("xesam:title", text("Song")),
                    ("xesam:artist", Field::Texts(vec!["A".into(), "B".into()])),
                    ("xesam:album", text("Album")),
                    ("mpris:artUrl", text("file:///tmp/cover.png")),
                    ("mpris:length", Field::Integer(200_000_000)),
                ],
            ),
        )
        .expect("a playing player has a track");
        assert_eq!(
            read.session,
            MediaSession {
                app_id: NAME.to_owned(),
                title: "Song".to_owned(),
                artist: "A, B".to_owned(),
                album: "Album".to_owned(),
                playing: true,
                paused: false,
            }
        );
        assert_eq!(read.art_url.as_deref(), Some("file:///tmp/cover.png"));
        assert_eq!(
            read.timeline,
            Some(RawTimeline {
                start: 0,
                end: 2_000_000_000,
                position: 500_000_000,
                updated: 0,
            })
        );
        assert!(read.controls.allows(Transport::Previous));
        assert!(read.controls.allows(Transport::PlayPause));
        assert!(!read.controls.allows(Transport::Next));
    }

    #[test]
    fn a_stopped_player_or_one_without_a_status_has_no_track() {
        let metadata = || vec![("xesam:title", text("Song"))];
        assert!(player(NAME, &song("Stopped", metadata())).is_none());
        assert!(player(NAME, &song("Buffering", metadata())).is_none());
        let mut silent = song("Paused", metadata());
        silent.remove("PlaybackStatus");
        assert!(player(NAME, &silent).is_none());
        let paused = player(NAME, &song("Paused", metadata())).unwrap();
        assert!(!paused.session.playing);
    }

    #[test]
    fn missing_or_wrong_typed_values_count_as_absent() {
        let read = player(
            NAME,
            &song(
                "Playing",
                vec![
                    ("xesam:title", Field::Integer(3)),
                    ("xesam:artist", Field::Other),
                    ("mpris:artUrl", Field::Texts(vec!["file:///a.png".into()])),
                    ("mpris:length", text("200000000")),
                ],
            ),
        )
        .unwrap();
        assert_eq!(
            (
                read.session.title.as_str(),
                read.session.artist.as_str(),
                read.session.album.as_str()
            ),
            ("", "", "")
        );
        assert_eq!((read.art_url, read.timeline), (None, None));
        let bare = player(NAME, &properties(vec![("PlaybackStatus", text("Playing"))])).unwrap();
        assert_eq!(bare.controls, Controls::default(), "no button without Can*");
        assert_eq!(bare.session.title, "");
    }

    #[test]
    fn a_track_without_a_length_or_position_has_no_timeline() {
        let live = song("Playing", vec![("mpris:length", Field::Integer(0))]);
        assert_eq!(player(NAME, &live).unwrap().timeline, None);
        let mut unplaced = song("Playing", vec![("mpris:length", Field::Integer(9))]);
        unplaced.insert("Position".to_owned(), Field::Other);
        assert_eq!(player(NAME, &unplaced).unwrap().timeline, None);
    }

    #[test]
    fn one_artist_sent_as_text_still_shows() {
        let read = player(NAME, &song("Playing", vec![("xesam:artist", text("Solo"))])).unwrap();
        assert_eq!(read.session.artist, "Solo");
    }

    #[test]
    fn only_local_file_urls_name_artwork() {
        let path = |url: &str| art_path(url).map(|path| path.to_string_lossy().into_owned());
        assert_eq!(
            path("file:///home/me/Music/cover%20art.jpg").as_deref(),
            Some("/home/me/Music/cover art.jpg")
        );
        assert_eq!(
            path("file://localhost/tmp/a%C3%A9.png").as_deref(),
            Some("/tmp/a\u{e9}.png")
        );
        assert_eq!(
            path("file:///tmp/a.png?size=1").as_deref(),
            Some("/tmp/a.png")
        );
        assert_eq!(path("https://example.com/a.png"), None);
        assert_eq!(path("file://server/share/a.png"), None);
        assert_eq!(path("file:///tmp/%zz.png"), None);
        assert_eq!(path("file:///tmp/%FF.png"), None, "not UTF-8");
    }
}
