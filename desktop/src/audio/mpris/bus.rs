//! The MPRIS players on the session bus, listed and driven through zbus's
//! blocking API. The connection sets [`APP_WAIT`] as its method call
//! timeout, so a player that hangs costs the reading thread that wait and no
//! more; proxies never cache, so every reading asks the player.

use std::collections::HashMap;
use std::fmt::Display;
use std::fs::File;
use std::io::Read;
use std::sync::Arc;
use std::time::Instant;

use zbus::blocking::Connection;
use zbus::blocking::fdo::{DBusProxy, PropertiesProxy};
use zbus::names::{BusName, InterfaceName};
use zbus::proxy::CacheProperties;
use zbus::zvariant::Value;

use super::artwork;
use super::fields::{self, Field, Player};
use super::owner;
use crate::audio::media_sessions::{
    APP_WAIT, MediaCardReader, MediaSessionReader, SessionList, THUMBNAIL_LIMIT, spawn,
};
use crate::audio::now_playing::{Follow, MediaSession, POLL_INTERVAL, RawTimeline, pick};
use crate::debug_log::DebugLog;
use crate::now_playing_card::{CardArtwork, MPRIS_PREFIX, Transport};

pub(super) const OBJECT_PATH: &str = "/org/mpris/MediaPlayer2";
const ROOT_INTERFACE: &str = "org.mpris.MediaPlayer2";
const PLAYER_INTERFACE: &str = "org.mpris.MediaPlayer2.Player";

/// Starts the thread that reads the MPRIS players for both readers.
// Unused until the Linux backend hands the app its media parts.
#[cfg_attr(not(test), allow(dead_code))]
pub fn read_media_sessions(log: DebugLog) -> (MediaSessionReader, MediaCardReader) {
    spawn(
        || Ok(Players::new(MPRIS_PREFIX, Bus::Session)),
        POLL_INTERVAL,
        log,
    )
}

/// Which bus the players are on.
pub(super) enum Bus {
    Session,
    /// A bus by its address, for the tests.
    #[cfg(test)]
    At(String),
}

/// The players whose bus names start with `prefix`, over a connection made
/// when first needed and again after a failed listing.
pub(super) struct Players {
    prefix: String,
    bus: Bus,
    connection: Option<Connection>,
}

/// One listed player.
pub(super) struct Entry {
    name: String,
    player: Player,
    connection: Connection,
}

impl Players {
    pub(super) fn new(prefix: &str, bus: Bus) -> Players {
        Players {
            prefix: prefix.to_owned(),
            bus,
            connection: None,
        }
    }

    fn connection(&mut self) -> Result<Connection, String> {
        if let Some(connection) = &self.connection {
            return Ok(connection.clone());
        }
        let builder = match &self.bus {
            Bus::Session => zbus::blocking::connection::Builder::session(),
            #[cfg(test)]
            Bus::At(address) => zbus::blocking::connection::Builder::address(address.as_str()),
        };
        let connection = builder
            .and_then(|builder| builder.method_timeout(APP_WAIT).build())
            .map_err(|error| format!("the session bus can't be reached: {error}"))?;
        self.connection = Some(connection.clone());
        Ok(connection)
    }
}

impl Entry {
    /// The process behind the player's bus connection.
    fn pid(&self) -> Option<u32> {
        let name = BusName::try_from(self.name.as_str()).ok()?;
        dbus(&self.connection)
            .ok()?
            .get_connection_unix_process_id(name)
            .ok()
    }

    /// The desktop entry the player names, without `.desktop`.
    fn desktop_entry(&self) -> Option<String> {
        let interface = InterfaceName::try_from(ROOT_INTERFACE).ok()?;
        let value = properties_proxy(&self.connection, &self.name)
            .ok()?
            .get(interface, "DesktopEntry")
            .ok()?;
        match field(&value) {
            Field::Text(entry) => Some(entry),
            _ => None,
        }
    }
}

impl SessionList for Players {
    type Session = Entry;

    /// The players that have a current track, by bus name. A player that
    /// fails to answer is left out.
    fn list(&mut self) -> Result<Vec<(MediaSession, Entry)>, String> {
        let connection = self.connection()?;
        let mut names: Vec<String> = dbus(&connection)?
            .list_names()
            .map_err(message)?
            .into_iter()
            .map(|name| name.to_string())
            .filter(|name| name.starts_with(&self.prefix))
            .collect();
        names.sort();
        Ok(names
            .into_iter()
            .filter_map(|name| {
                let properties = player_properties(&connection, &name).ok()?;
                let player = fields::player(&name, &properties)?;
                let entry = Entry {
                    name,
                    player,
                    connection: connection.clone(),
                };
                Some((entry.player.session.clone(), entry))
            })
            .collect())
    }

    /// For one app, the players of its process or a parent's, else those
    /// named for its executable; among them the usual pick.
    fn pick(&self, listed: &[(MediaSession, Entry)], follow: &Follow) -> Option<usize> {
        let mine: Vec<usize> = match follow {
            Follow::AnyApp => (0..listed.len()).collect(),
            Follow::App { executable, pid } => {
                let lineage = pid
                    .map(|pid| owner::lineage(pid, owner::proc_stat))
                    .unwrap_or_default();
                owner::belonging(
                    listed.len(),
                    |index| {
                        !lineage.is_empty()
                            && listed[index]
                                .1
                                .pid()
                                .is_some_and(|pid| lineage.contains(&pid))
                    },
                    |index| {
                        let entry = &listed[index].1;
                        owner::named_for(&entry.name, entry.desktop_entry().as_deref(), executable)
                    },
                )
            }
        };
        let described: Vec<MediaSession> =
            mine.iter().map(|&index| listed[index].0.clone()).collect();
        pick(&described, &Follow::AnyApp).map(|found| mine[found])
    }

    fn thumbnail(&self, entry: &Entry, _deadline: Instant) -> Result<Option<Vec<u8>>, String> {
        let Some(path) = entry.player.art_url.as_deref().and_then(fields::art_path) else {
            return Ok(None);
        };
        let mut image = Vec::new();
        File::open(path)
            .and_then(|file| file.take(THUMBNAIL_LIMIT + 1).read_to_end(&mut image))
            .map_err(message)?;
        if image.len() as u64 > THUMBNAIL_LIMIT {
            return Err(format!("the artwork has over {THUMBNAIL_LIMIT} bytes"));
        }
        Ok((!image.is_empty()).then_some(image))
    }

    fn timeline(&self, entry: &Entry) -> Result<RawTimeline, String> {
        entry
            .player
            .timeline
            .ok_or_else(|| "the track has no length".to_owned())
    }

    // The connection's method timeout is the deadline the thread gives.
    fn send(&self, entry: &Entry, command: Transport, _deadline: Instant) -> Result<bool, String> {
        if !entry.player.controls.allows(command) {
            return Ok(false);
        }
        let method = match command {
            Transport::Previous => "Previous",
            Transport::PlayPause => "PlayPause",
            Transport::Next => "Next",
        };
        entry
            .connection
            .call_method(
                Some(entry.name.as_str()),
                OBJECT_PATH,
                Some(PLAYER_INTERFACE),
                method,
                &(),
            )
            .map_err(message)?;
        Ok(true)
    }

    fn reset(&mut self) {
        self.connection = None;
    }

    fn fit_image(&self, image: &[u8]) -> Result<Option<Vec<u8>>, String> {
        artwork::fit_image(image)
    }

    fn card_artwork(&self, image: &[u8]) -> Result<Option<CardArtwork>, String> {
        artwork::card_artwork(image)
    }

    fn icon(&self, log: &DebugLog) -> Option<Arc<[u8]>> {
        artwork::fitted_icon(log)
    }
}

fn dbus(connection: &Connection) -> Result<DBusProxy<'static>, String> {
    DBusProxy::builder(connection)
        .cache_properties(CacheProperties::No)
        .build()
        .map_err(message)
}

fn properties_proxy<'a>(
    connection: &Connection,
    name: &'a str,
) -> Result<PropertiesProxy<'a>, String> {
    PropertiesProxy::builder(connection)
        .destination(name)
        .and_then(|builder| builder.path(OBJECT_PATH))
        .and_then(|builder| builder.cache_properties(CacheProperties::No).build())
        .map_err(message)
}

/// Every property of the player's `Player` interface, in one call.
fn player_properties(
    connection: &Connection,
    name: &str,
) -> Result<HashMap<String, Field>, String> {
    let interface = InterfaceName::try_from(PLAYER_INTERFACE).map_err(message)?;
    let values = properties_proxy(connection, name)?
        .get_all(interface)
        .map_err(message)?;
    Ok(values
        .iter()
        .map(|(key, value)| (key.clone(), field(value)))
        .collect())
}

/// `value` as the mapping reads it: any integer type as an integer, a
/// variant as what it holds.
pub(super) fn field(value: &Value<'_>) -> Field {
    match value {
        Value::Value(inner) => field(inner),
        Value::Str(text) => Field::Text(text.to_string()),
        Value::ObjectPath(path) => Field::Text(path.to_string()),
        Value::Bool(flag) => Field::Bool(*flag),
        Value::U8(number) => Field::Integer(i64::from(*number)),
        Value::I16(number) => Field::Integer(i64::from(*number)),
        Value::U16(number) => Field::Integer(i64::from(*number)),
        Value::I32(number) => Field::Integer(i64::from(*number)),
        Value::U32(number) => Field::Integer(i64::from(*number)),
        Value::I64(number) => Field::Integer(*number),
        Value::U64(number) => i64::try_from(*number).map_or(Field::Other, Field::Integer),
        Value::Array(array) => array
            .inner()
            .iter()
            .map(|item| match field(item) {
                Field::Text(text) => Some(text),
                _ => None,
            })
            .collect::<Option<Vec<String>>>()
            .map_or(Field::Other, Field::Texts),
        Value::Dict(dict) => Field::Map(
            dict.iter()
                .filter_map(|(key, value)| match key {
                    Value::Str(key) => Some((key.to_string(), field(value))),
                    _ => None,
                })
                .collect(),
        ),
        _ => Field::Other,
    }
}

fn message(error: impl Display) -> String {
    error.to_string()
}
