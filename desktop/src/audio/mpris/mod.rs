//! Now playing on Linux: the MPRIS media players on the session bus, as a
//! [`SessionList`](crate::audio::media_sessions::SessionList) for the shared
//! reading thread ([`crate::audio::media_sessions`]).
//!
//! Players are polled like Windows' sessions: at each poll one `GetAll` on
//! every player's `Player` interface, all sent at once so the listing is
//! bounded by one [`APP_WAIT`](crate::audio::media_sessions::APP_WAIT)
//! however many players hang, and never answered from a cache. They are
//! ordered by bus name, so the pick holds between polls; a stopped player
//! has no track ([`fields`]). With one app as the source, a player counts
//! when its process is the source's process or one of its parents, else
//! when its desktop entry or bus name names the source's executable
//! ([`owner`]). Artwork is read from `file://` URLs only: fetching a web
//! URL would have AudioSync open connections on its own. Without a session
//! bus every reading is no track, and the failure is logged once.

#[cfg(target_os = "linux")]
mod artwork;
#[cfg(target_os = "linux")]
mod bus;
mod fields;
mod owner;
#[cfg(all(test, target_os = "linux"))]
mod tests;

// Unused until the Linux backend hands the app its media parts.
#[cfg(target_os = "linux")]
#[cfg_attr(not(test), allow(unused_imports))]
pub use bus::read_media_sessions;
