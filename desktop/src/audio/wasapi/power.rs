//! Windows' suspend and resume notices (engine spec S332), from one
//! registration for the whole process. The callback runs on a system thread
//! and only counts; the controller takes the counts at its next poll.

use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE};
use windows::Win32::System::Power::{
    DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, PowerRegisterSuspendResumeNotification,
};
use windows::Win32::System::WindowsProgramming::QueryUnbiasedInterruptTime;
use windows::Win32::UI::WindowsAndMessaging::{
    DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND,
};

use crate::audio::controller::PowerNotice;
use crate::debug_log::DebugLog;

static SUSPENDS: AtomicU32 = AtomicU32::new(0);
static RESUMES: AtomicU32 = AtomicU32::new(0);

unsafe extern "system" fn on_power(
    _context: *const c_void,
    kind: u32,
    _setting: *const c_void,
) -> u32 {
    match kind {
        PBT_APMSUSPEND => {
            SUSPENDS.fetch_add(1, Ordering::Relaxed);
        }
        PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => {
            RESUMES.fetch_add(1, Ordering::Relaxed);
        }
        _ => {}
    }
    ERROR_SUCCESS.0
}

/// The notices counted since the last call.
pub struct PowerWatch {
    suspends: u32,
    resumes: u32,
}

impl PowerWatch {
    /// Registers for the notices once per process; the registration lasts
    /// until it exits. A refused one is logged: the controller still finds a
    /// resume by the clock.
    pub fn start() -> PowerWatch {
        static REGISTERED: OnceLock<()> = OnceLock::new();
        REGISTERED.get_or_init(|| {
            // Windows may keep the parameters for as long as the
            // registration lasts, which is the process's life.
            let parameters = Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
                Callback: Some(on_power),
                Context: std::ptr::null_mut(),
            }));
            let mut handle = std::ptr::null_mut();
            // SAFETY: the parameters live for the rest of the process, and
            // the callback only touches atomics.
            let result = unsafe {
                PowerRegisterSuspendResumeNotification(
                    DEVICE_NOTIFY_CALLBACK,
                    HANDLE(std::ptr::from_mut(parameters).cast()),
                    &raw mut handle,
                )
            };
            if result != ERROR_SUCCESS {
                let log = DebugLog::appending_from_env();
                log.log(|| {
                    format!(
                        "Windows refused the suspend and resume notices (error {}); a wake is \
                         found by the clock only",
                        result.0
                    )
                });
                log.flush(Duration::from_secs(1));
            }
        });
        PowerWatch {
            suspends: SUSPENDS.load(Ordering::Relaxed),
            resumes: RESUMES.load(Ordering::Relaxed),
        }
    }

    /// The notices since the last call. Both kinds since then mean the PC
    /// slept and woke between two polls.
    pub fn take(&mut self) -> Vec<PowerNotice> {
        let suspends = SUSPENDS.load(Ordering::Relaxed);
        let resumes = RESUMES.load(Ordering::Relaxed);
        let mut notices = Vec::new();
        if suspends != self.suspends {
            notices.push(PowerNotice::Suspend);
        }
        if resumes != self.resumes {
            notices.push(PowerNotice::Resume);
        }
        self.suspends = suspends;
        self.resumes = resumes;
        notices
    }
}

/// Time since boot less the time the PC slept or hibernated
/// (`QueryUnbiasedInterruptTime`, in 100 ns units), so a UI loop held while
/// the PC stays awake isn't taken for a sleep (S332).
pub fn awake_clock() -> Duration {
    let mut ticks = 0u64;
    // SAFETY: the call only writes the count through the pointer it gets.
    let _ = unsafe { QueryUnbiasedInterruptTime(&raw mut ticks) };
    Duration::from_nanos(ticks.saturating_mul(100))
}
