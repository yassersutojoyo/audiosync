//! AudioSync's platform-independent audio core.
//!
//! Written from `docs/ENGINE_SPEC.md`; clause IDs (S1, S2, ...) in comments and
//! test names refer to that document. No OS audio API is used here: the host
//! (the macOS app or the Windows app) owns capture and output devices, and
//! calls into this crate a whole buffer at a time.

pub mod airplay2_identity;
pub mod airplay_pairings;
pub mod calibrate;
pub mod calibrate_all;
pub mod channels;
pub mod clock;
pub mod convert;
pub mod delay_line;
pub mod eq;
pub mod fade;
pub mod ffi;
pub mod lead;
pub mod meter;
#[cfg(feature = "ffi-panic-probe")]
pub mod panic_probe;
pub mod policy;
pub mod reader;
pub mod render;
pub mod resample;
pub mod ring;
pub mod volume;

/// The crate version, also exported through the C interface.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
