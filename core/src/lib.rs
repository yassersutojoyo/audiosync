//! AudioSync's platform-independent audio core.
//!
//! Written from `docs/ENGINE_SPEC.md`; clause IDs (S1, S2, ...) in comments and
//! test names refer to that document. No OS audio API is used here: the host
//! (the macOS app, later the Windows app) owns capture, output devices and the
//! pipe, and calls into this crate a whole buffer at a time.

pub mod channels;
pub mod convert;
pub mod eq;
pub mod ffi;
pub mod fifo;
pub mod policy;
pub mod reader;
pub mod resample;
pub mod ring;
pub mod volume;

/// The crate version, also exported through the C interface.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
