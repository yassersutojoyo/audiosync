//! AudioSync's platform-independent audio core.
//!
//! Written from `docs/ENGINE_SPEC.md`; clause IDs (S1, S2, ...) in comments and
//! test names refer to that document. No OS audio API is used here: the host
//! (the macOS app, later the Windows app) owns capture, output devices and the
//! pipe, and calls into this crate a whole buffer at a time.

pub mod convert;
pub mod fifo;
pub mod reader;
pub mod resample;
pub mod ring;
