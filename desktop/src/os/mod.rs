//! What differs between the systems the app runs on, behind one face for
//! `main`: [`start`] opens the audio backend and what feeds it, [`Instance`]
//! is the single-instance lock and [`create_tray`] the notification-area
//! icon. The data folder's base, the pairing file's sealer, the computer's
//! name and the notice clock come from here too. `windows` is the Windows 11
//! app; `other` is the UI-only build on hosts without a backend yet, which
//! on Linux takes the instance lock, the sealer and the data folder's base
//! from `xdg`.

#[cfg(any(windows, target_os = "linux"))]
mod claim;
#[cfg(any(target_os = "linux", test))]
mod desktop_entry;
#[cfg(os_backend)]
mod engine;
#[cfg(not(windows))]
mod other;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "linux")]
pub mod xdg;

#[cfg(not(windows))]
use self::other as imp;
#[cfg(windows)]
use self::windows as imp;

#[cfg(os_backend)]
pub use imp::data_base;
#[cfg(feature = "mock-backend")]
pub use imp::start_with;
pub use imp::{
    Instance, Media, claim_instance, computer_name, create_tray, notice_time, sealer, start,
};

/// What [`start`] opened.
pub struct Started {
    /// What the windows show and drive.
    pub backend: Box<dyn crate::ui_model::Backend>,
    /// What the backend hands the app once its windows exist.
    pub media: Media,
}
