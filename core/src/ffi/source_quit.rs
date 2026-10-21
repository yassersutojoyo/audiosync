//! What a session does when the app it captures quits (S227), through the C
//! interface: the host passes the stored setting and source and the bundle ID
//! of the app that quit, and gets the action back.
//!
//! The rules are in [`source_quit`]; this module only turns C strings and
//! codes into its types. For the host's control thread: it may allocate.

use std::ffi::{CStr, c_char};

use crate::policy::source::AudioSource;
use crate::policy::source_quit::{self, SourceQuitAction, SourceQuitWatch, WhenSourceQuits};

/// [`SourceQuitAction::Keep`]: nothing changes.
pub const SOURCE_QUIT_KEEP: u32 = 0;
/// [`SourceQuitAction::Stop`]: stop as S142 says, dropping a pending rebuild.
pub const SOURCE_QUIT_STOP: u32 = 1;
/// [`SourceQuitAction::SwitchToSystem`]: go on with whole-system audio.
pub const SOURCE_QUIT_SWITCH_TO_SYSTEM: u32 = 2;

/// [`source_quit::Streaming::Off`]: no session runs.
pub const STREAMING_OFF: u32 = 0;
/// [`source_quit::Streaming::Running`]: a session runs.
pub const STREAMING_RUNNING: u32 = 1;
/// [`source_quit::Streaming::Rebuilding`]: a session is being rebuilt.
pub const STREAMING_REBUILDING: u32 = 2;

/// # Safety
///
/// `text` is null or points to a NUL-terminated string that stays valid and
/// unchanged during the call.
unsafe fn host_text<'a>(text: *const c_char) -> Option<&'a str> {
    if text.is_null() {
        return None;
    }
    // SAFETY: non-null, and by the caller's contract NUL-terminated.
    unsafe { CStr::from_ptr(text) }.to_str().ok()
}

/// What the host does about `quit_app` quitting ([`SourceQuitWatch::app_exited`],
/// S164, S227): `setting` and `source` in their stored forms (null for
/// nothing stored), `streaming` one of the `STREAMING_` codes. A string that
/// isn't UTF-8 counts as null and an unknown `streaming` code as off, so a bad
/// argument never switches anything.
///
/// # Safety
///
/// Each pointer is null or points to a NUL-terminated string that stays
/// valid and unchanged during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_source_quit_decide(
    setting: *const c_char,
    source: *const c_char,
    quit_app: *const c_char,
    streaming: u32,
) -> u32 {
    // SAFETY: by the caller's contract each is null or a NUL-terminated string.
    let (setting, source, quit_app) =
        unsafe { (host_text(setting), host_text(source), host_text(quit_app)) };
    let streaming = match streaming {
        STREAMING_RUNNING => source_quit::Streaming::Running,
        STREAMING_REBUILDING => source_quit::Streaming::Rebuilding,
        _ => source_quit::Streaming::Off,
    };
    let mut watch = SourceQuitWatch::new(WhenSourceQuits::from_stored(setting));
    match watch.app_exited(&AudioSource::from_stored(source), quit_app, streaming) {
        SourceQuitAction::Keep => SOURCE_QUIT_KEEP,
        SourceQuitAction::Stop => SOURCE_QUIT_STOP,
        SourceQuitAction::SwitchToSystem => SOURCE_QUIT_SWITCH_TO_SYSTEM,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decide(setting: Option<&CStr>, source: &CStr, quit: Option<&CStr>, streaming: u32) -> u32 {
        let ptr = |text: Option<&CStr>| text.map_or(std::ptr::null(), CStr::as_ptr);
        // SAFETY: every pointer is null or a live C string literal.
        unsafe { asc_source_quit_decide(ptr(setting), source.as_ptr(), ptr(quit), streaming) }
    }

    // covers: S142, S164, S227
    #[test]
    fn the_decision_crosses_the_boundary_as_the_core_makes_it() {
        let player = c"com.example.player";
        for streaming in [STREAMING_RUNNING, STREAMING_REBUILDING] {
            assert_eq!(
                decide(None, player, Some(player), streaming),
                SOURCE_QUIT_STOP
            );
            assert_eq!(
                decide(Some(c"stop"), player, Some(player), streaming),
                SOURCE_QUIT_STOP
            );
            assert_eq!(
                decide(Some(c"system"), player, Some(player), streaming),
                SOURCE_QUIT_SWITCH_TO_SYSTEM
            );
        }
        assert_eq!(
            decide(Some(c"system"), player, Some(player), STREAMING_OFF),
            SOURCE_QUIT_KEEP
        );
        assert_eq!(
            decide(
                Some(c"system"),
                player,
                Some(c"com.example.other"),
                STREAMING_RUNNING
            ),
            SOURCE_QUIT_KEEP
        );
        assert_eq!(
            decide(Some(c"system"), player, None, STREAMING_RUNNING),
            SOURCE_QUIT_KEEP
        );
        assert_eq!(
            decide(Some(c"system"), c"", Some(player), STREAMING_RUNNING),
            SOURCE_QUIT_KEEP
        );
        assert_eq!(
            decide(
                Some(c"system"),
                c"input:mic",
                Some(c"mic"),
                STREAMING_RUNNING
            ),
            SOURCE_QUIT_KEEP
        );
    }

    // covers: S227
    #[test]
    fn bad_arguments_never_switch() {
        let player = c"com.example.player";
        let not_utf8 = c"\xff\xfe";
        assert_eq!(
            decide(Some(c"system"), player, Some(player), 7),
            SOURCE_QUIT_KEEP
        );
        assert_eq!(
            decide(Some(not_utf8), player, Some(player), STREAMING_RUNNING),
            SOURCE_QUIT_STOP
        );
        assert_eq!(
            decide(Some(c"system"), not_utf8, Some(not_utf8), STREAMING_RUNNING),
            SOURCE_QUIT_KEEP
        );
        // SAFETY: null pointers are allowed.
        let all_null = unsafe {
            asc_source_quit_decide(
                std::ptr::null(),
                std::ptr::null(),
                std::ptr::null(),
                STREAMING_RUNNING,
            )
        };
        assert_eq!(all_null, SOURCE_QUIT_KEEP);
    }
}
