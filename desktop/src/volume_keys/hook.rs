//! The low-level keyboard hook that takes the volume keys while they move the
//! master (spec S429). It runs on a thread of its own, whose message loop is
//! what Windows calls the hook through; the hook only posts the key to that
//! loop, which hands it to the UI thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::SystemInformation::GetTickCount;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    VIRTUAL_KEY, VK_VOLUME_DOWN, VK_VOLUME_MUTE, VK_VOLUME_UP,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, HC_ACTION, HHOOK, KBDLLHOOKSTRUCT, MSG, PM_NOREMOVE, PeekMessageW,
    PostThreadMessageW, SetTimer, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL, WM_APP,
    WM_KEYDOWN, WM_QUIT, WM_SYSKEYDOWN, WM_TIMER,
};

use super::VolumeKey;
use crate::audio::controller::PowerNotice;
use crate::audio::wasapi::power::PowerWatch;
use crate::debug_log::DebugLog;

/// A key the hook took, posted to its own thread's loop.
const WM_VOLUME_KEY: u32 = WM_APP + 1;
/// How late a key may reach the hook before it counts as one Windows may
/// have removed for running past its timeout.
const LATE_MS: u32 = 500;
/// How often the loop looks for a wake or a late key.
const CHECK_MS: u32 = 1000;

static LATE: AtomicBool = AtomicBool::new(false);
static MUTE_HELD: AtomicBool = AtomicBool::new(false);

/// The hook while the keys are taken; dropping it gives them back.
pub struct KeyHook {
    /// The hook thread's ID, or the channel it still comes on when the
    /// thread was slow to install the hook: it may install it later.
    thread_id: Result<u32, Receiver<u32>>,
    thread: Option<JoinHandle<()>>,
}

impl KeyHook {
    /// Starts the hook's thread; `deliver` runs on that thread for every key
    /// press the hook took, and should only pass it on.
    pub fn start(deliver: fn(VolumeKey), log: DebugLog) -> Option<KeyHook> {
        let (started, thread_id) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("audiosync-volume-keys".to_owned())
            .spawn(move || run(&started, deliver, &log))
            .ok()?;
        let thread_id = match thread_id.recv_timeout(Duration::from_secs(5)) {
            Ok(id) => Ok(id),
            Err(RecvTimeoutError::Timeout) => Err(thread_id),
            Err(RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
                return None;
            }
        };
        Some(KeyHook {
            thread_id,
            thread: Some(thread),
        })
    }
}

impl Drop for KeyHook {
    fn drop(&mut self) {
        let thread_id = match &self.thread_id {
            Ok(id) => Some(*id),
            Err(started) => started.recv().ok(),
        };
        if let Some(thread_id) = thread_id {
            // SAFETY: posting to a thread ID only queues a message; a thread
            // that already ended makes it fail, which changes nothing.
            let _ = unsafe { PostThreadMessageW(thread_id, WM_QUIT, WPARAM(0), LPARAM(0)) };
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn install(log: &DebugLog) -> Option<HHOOK> {
    // SAFETY: `on_key` lives for the whole process, and a low-level hook is
    // called on this thread, which runs a message loop until it unhooks.
    match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(on_key), None, 0) } {
        Ok(hook) => Some(hook),
        Err(error) => {
            log.log(|| format!("volume keys: the keyboard hook did not install ({error})"));
            None
        }
    }
}

fn uninstall(hook: Option<HHOOK>) {
    if let Some(hook) = hook {
        // SAFETY: `hook` came from SetWindowsHookExW on this thread.
        let _ = unsafe { UnhookWindowsHookEx(hook) };
    }
}

fn run(started: &std::sync::mpsc::Sender<u32>, deliver: fn(VolumeKey), log: &DebugLog) {
    let mut msg = MSG::default();
    // SAFETY: plain Win32 calls on this thread; the first one makes its
    // message queue, so the quit posted by `Drop` can't be lost.
    let thread_id = unsafe {
        let _ = PeekMessageW(&raw mut msg, None, 0, 0, PM_NOREMOVE);
        SetTimer(None, 0, CHECK_MS, None);
        GetCurrentThreadId()
    };
    LATE.store(false, Ordering::Relaxed);
    MUTE_HELD.store(false, Ordering::Relaxed);
    let mut hook = install(log);
    if hook.is_some() {
        log.log(|| "volume keys: the keyboard hook takes the volume keys".to_owned());
    }
    let _ = started.send(thread_id);
    let mut power = PowerWatch::start();
    // SAFETY: `msg` is a writable MSG for this thread's queue.
    while unsafe { GetMessageW(&raw mut msg, None, 0, 0) }.as_bool() {
        match msg.message {
            WM_VOLUME_KEY => match msg.wParam.0 {
                0 => deliver(VolumeKey::Up),
                1 => deliver(VolumeKey::Down),
                _ => deliver(VolumeKey::Mute),
            },
            WM_TIMER => {
                let woke = power
                    .take()
                    .iter()
                    .any(|notice| matches!(notice, PowerNotice::Resume));
                let late = LATE.swap(false, Ordering::Relaxed);
                if woke || late {
                    uninstall(hook);
                    hook = install(log);
                    let why = if woke {
                        "after a wake"
                    } else {
                        "after a key reached it late"
                    };
                    if hook.is_some() {
                        log.log(|| format!("volume keys: the keyboard hook installed again {why}"));
                    }
                }
            }
            _ => {}
        }
    }
    uninstall(hook);
    log.log(|| "volume keys: the keyboard hook gave the volume keys back".to_owned());
}

/// Takes volume up, volume down and mute, down and up, and passes every other
/// key on at once.
unsafe extern "system" fn on_key(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION, `lparam` points to the key's KBDLLHOOKSTRUCT.
        let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let key = match VIRTUAL_KEY(info.vkCode as u16) {
            VK_VOLUME_UP => Some(0),
            VK_VOLUME_DOWN => Some(1),
            VK_VOLUME_MUTE => Some(2),
            _ => None,
        };
        if let Some(key) = key {
            let down = matches!(wparam.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            // A held mute key repeats; it toggles once per press.
            let acts = if key == 2 {
                let held = MUTE_HELD.swap(down, Ordering::Relaxed);
                down && !held
            } else {
                down
            };
            if acts {
                // SAFETY: posting to this thread's own queue only queues a message.
                let _ = unsafe {
                    PostThreadMessageW(GetCurrentThreadId(), WM_VOLUME_KEY, WPARAM(key), LPARAM(0))
                };
            }
            // SAFETY: GetTickCount only reads the tick count.
            if unsafe { GetTickCount() }.wrapping_sub(info.time) >= LATE_MS {
                LATE.store(true, Ordering::Relaxed);
            }
            return LRESULT(1);
        }
    }
    // SAFETY: the arguments are the ones Windows gave this hook.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}
