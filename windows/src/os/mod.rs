//! What differs between the systems the app runs on, behind one face for
//! `main`: [`start`] opens the audio backend and what feeds it, [`Instance`]
//! is the single-instance lock and [`create_tray`] the notification-area
//! icon. The data folder's base, the pairing file's sealer, the computer's
//! name and the notice clock come from here too. `windows` is the Windows 11
//! app; `other` is the UI-only build on hosts without a backend yet.

#[cfg(os_backend)]
mod engine;
#[cfg(not(windows))]
mod other;
#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
use self::other as imp;
#[cfg(windows)]
use self::windows as imp;

pub use imp::{
    Instance, Media, claim_instance, computer_name, create_tray, data_base, notice_time, sealer,
    start,
};

/// What [`start`] opened.
pub struct Started {
    /// What the windows show and drive.
    pub backend: Box<dyn crate::ui_model::Backend>,
    /// What the backend hands the app once its windows exist.
    pub media: Media,
}
