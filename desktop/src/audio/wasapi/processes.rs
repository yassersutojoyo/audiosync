//! The running processes, the audio sessions they own, and watching a process
//! for its exit.

use windows::Win32::Foundation::{
    ERROR_INVALID_PARAMETER, FILETIME, HWND, LPARAM, S_OK, WAIT_OBJECT_0,
};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::Media::Audio::{
    AudioSessionStateExpired, DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2,
    eRender,
};
use windows::Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
};
use windows::Win32::Storage::Packaging::Appx::GetApplicationUserModelId;
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SYNCHRONIZE, QueryFullProcessImageNameW, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{IShellItem, SHCreateItemFromParsingName, SIGDN_NORMALDISPLAY};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GW_OWNER, GWL_EXSTYLE, GetWindow, GetWindowLongPtrW, GetWindowTextLengthW,
    GetWindowThreadProcessId, IsWindowVisible, WS_EX_TOOLWINDOW,
};
use windows::core::{BOOL, HRESULT, HSTRING, Interface, PCWSTR, PWSTR};

use super::com::{Apartment, Handle, system_error, take_com_string};
use super::devices::enumerator;
use crate::audio::backend::BackendError;
use crate::audio::process::{ProcessEntry, ProcessTable};

/// A snapshot of the running processes.
pub(super) fn snapshot() -> Result<ProcessTable, BackendError> {
    // SAFETY: takes a snapshot of the process list; the handle is owned
    // below.
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }
        .map_err(|error| system_error("CreateToolhelp32Snapshot", &error))?;
    let snapshot = Handle::new(snapshot);
    let mut entry = PROCESSENTRY32W {
        dwSize: size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut entries = Vec::new();
    // SAFETY: `entry` is a writable PROCESSENTRY32W whose size field is set,
    // as both calls require.
    let mut found = unsafe { Process32FirstW(snapshot.raw(), &mut entry) }.is_ok();
    while found {
        entries.push(ProcessEntry {
            pid: entry.th32ProcessID,
            parent_pid: entry.th32ParentProcessID,
            created: creation_time(entry.th32ProcessID),
            executable: executable_name(&entry.szExeFile),
        });
        // SAFETY: as above.
        found = unsafe { Process32NextW(snapshot.raw(), &mut entry) }.is_ok();
    }
    Ok(ProcessTable::new(entries))
}

/// The text of a NUL-terminated UTF-16 buffer.
fn executable_name(wide: &[u16]) -> String {
    let length = wide
        .iter()
        .position(|&unit| unit == 0)
        .unwrap_or(wide.len());
    String::from_utf16_lossy(&wide[..length])
}

/// When the process was created, in file time ticks, if it can be queried.
/// A zero time is no creation time, so it doesn't count as the earliest.
fn creation_time(pid: u32) -> Option<u64> {
    if pid == 0 {
        return None;
    }
    // SAFETY: opens the process for limited queries; the handle is owned
    // below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let process = Handle::new(process);
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: a handle with query access and four writable FILETIMEs.
    unsafe {
        GetProcessTimes(
            process.raw(),
            &mut created,
            &mut exited,
            &mut kernel,
            &mut user,
        )
    }
    .ok()?;
    let created = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    (created != 0).then_some(created)
}

/// The IDs of the processes that own a visible, unowned top-level window
/// that has a title and is neither a tool window nor cloaked: the windows the
/// taskbar shows.
pub(super) fn window_pids() -> Vec<u32> {
    let mut pids: Vec<u32> = Vec::new();
    // SAFETY: the callback runs only during this call, while `pids` lives.
    let _ = unsafe { EnumWindows(Some(collect_window_pid), LPARAM(&raw mut pids as isize)) };
    pids.sort_unstable();
    pids.dedup();
    pids
}

unsafe extern "system" fn collect_window_pid(window: HWND, pids: LPARAM) -> BOOL {
    // SAFETY: queries on a window EnumWindows handed over; `pids` is the
    // vector `window_pids` passed, alive for the whole enumeration.
    unsafe {
        if !IsWindowVisible(window).as_bool()
            || GetWindow(window, GW_OWNER).is_ok()
            || GetWindowTextLengthW(window) == 0
            || GetWindowLongPtrW(window, GWL_EXSTYLE) & WS_EX_TOOLWINDOW.0 as isize != 0
        {
            return true.into();
        }
        let mut cloaked = 0u32;
        // Windows keeps suspended Store apps' windows visible but cloaked.
        if DwmGetWindowAttribute(
            window,
            DWMWA_CLOAKED,
            (&raw mut cloaked).cast(),
            size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
        {
            return true.into();
        }
        let mut pid = 0;
        GetWindowThreadProcessId(window, Some(&raw mut pid));
        if pid != 0 {
            (*(pids.0 as *mut Vec<u32>)).push(pid);
        }
    }
    true.into()
}

/// The name process `pid`'s app gives itself: a packaged app's name in the
/// Start menu, else its executable's file description, as Task Manager shows.
pub(super) fn app_name(pid: u32) -> Option<String> {
    // SAFETY: opens the process for limited queries; the handle is owned
    // below.
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let process = Handle::new(process);
    let mut wide = [0u16; 1024];
    let mut length = wide.len() as u32;
    // SAFETY: a handle with query access and a writable buffer of `length`
    // units.
    if unsafe {
        GetApplicationUserModelId(process.raw(), &mut length, Some(PWSTR(wide.as_mut_ptr())))
    }
    .is_ok()
        && let Some(name) = start_menu_name(&executable_name(&wide))
    {
        return Some(name);
    }
    let mut length = wide.len() as u32;
    // SAFETY: as above.
    unsafe {
        QueryFullProcessImageNameW(
            process.raw(),
            PROCESS_NAME_WIN32,
            PWSTR(wide.as_mut_ptr()),
            &mut length,
        )
    }
    .ok()?;
    file_description(&executable_name(&wide[..length as usize]))
}

/// The Start menu's name for the packaged app `app_id`.
fn start_menu_name(app_id: &str) -> Option<String> {
    let _apartment = Apartment::enter().ok()?;
    let path = HSTRING::from(format!("shell:AppsFolder\\{app_id}"));
    // SAFETY: a NUL-terminated path; the item is released on drop, and the
    // name COM allocated is taken and freed once.
    unsafe {
        let item: IShellItem = SHCreateItemFromParsingName(&path, None).ok()?;
        take_com_string(item.GetDisplayName(SIGDN_NORMALDISPLAY).ok()?)
    }
}

/// The file description in the version resource of the executable at `path`,
/// in the resource's first language.
fn file_description(path: &str) -> Option<String> {
    let path = HSTRING::from(path);
    // SAFETY: a NUL-terminated path and a buffer of the size Windows asked
    // for; the pointers VerQueryValueW returns point into that buffer.
    unsafe {
        let size = GetFileVersionInfoSizeW(&path, None);
        if size == 0 {
            return None;
        }
        let mut data = vec![0u8; size as usize];
        GetFileVersionInfoW(&path, None, size, data.as_mut_ptr().cast()).ok()?;
        let mut value = std::ptr::null_mut();
        let mut length = 0u32;
        if !VerQueryValueW(
            data.as_ptr().cast(),
            &HSTRING::from("\\VarFileInfo\\Translation"),
            &mut value,
            &mut length,
        )
        .as_bool()
            || length < 4
        {
            return None;
        }
        let language = *value.cast::<u16>();
        let code_page = *value.cast::<u16>().add(1);
        let key = format!("\\StringFileInfo\\{language:04x}{code_page:04x}\\FileDescription");
        if !VerQueryValueW(
            data.as_ptr().cast(),
            &HSTRING::from(key),
            &mut value,
            &mut length,
        )
        .as_bool()
            || length == 0
        {
            return None;
        }
        PCWSTR(value.cast()).to_string().ok()
    }
}

/// The IDs of the processes that own an audio session on an active render
/// endpoint. The system sounds session and expired sessions are left out.
pub(super) fn audio_session_pids() -> Result<Vec<u32>, BackendError> {
    let _apartment = Apartment::enter()?;
    let enumerator = enumerator()?;
    // SAFETY: calls on a live enumerator.
    let devices = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }
        .map_err(|error| system_error("IMMDeviceEnumerator::EnumAudioEndpoints", &error))?;
    // SAFETY: a call on the live collection.
    let count = unsafe { devices.GetCount() }
        .map_err(|error| system_error("IMMDeviceCollection::GetCount", &error))?;
    let mut pids = Vec::new();
    for index in 0..count {
        // SAFETY: calls on live COM objects this function owns; an endpoint
        // or session that fails is skipped.
        unsafe {
            let Ok(device) = devices.Item(index) else {
                continue;
            };
            let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                continue;
            };
            let Ok(sessions) = manager.GetSessionEnumerator() else {
                continue;
            };
            let Ok(total) = sessions.GetCount() else {
                continue;
            };
            for number in 0..total {
                let Ok(session) = sessions.GetSession(number) else {
                    continue;
                };
                if session
                    .GetState()
                    .is_ok_and(|state| state == AudioSessionStateExpired)
                {
                    continue;
                }
                let Ok(session) = session.cast::<IAudioSessionControl2>() else {
                    continue;
                };
                // The system sounds session belongs to no app.
                if session.IsSystemSoundsSession() == S_OK {
                    continue;
                }
                if let Ok(pid) = session.GetProcessId()
                    && pid != 0
                {
                    pids.push(pid);
                }
            }
        }
    }
    pids.sort_unstable();
    pids.dedup();
    Ok(pids)
}

/// `HRESULT_FROM_WIN32(ERROR_INVALID_PARAMETER)`: what opening a process ID
/// that doesn't exist gives.
const NO_SUCH_PROCESS: HRESULT = HRESULT::from_win32(ERROR_INVALID_PARAMETER.0);

/// A process watched for its exit.
pub struct ProcessWatch(Handle);

/// What asking to watch a running process gave.
pub(super) enum Watch {
    Watching(ProcessWatch),
    /// It runs but refused to be opened for a wait, with this error (an app
    /// running as administrator, say), so its exit goes unnoticed.
    Refused(HRESULT),
}

impl Watch {
    /// The watch, unless the process refused one.
    pub(super) fn watching(self) -> Option<ProcessWatch> {
        match self {
            Watch::Watching(watch) => Some(watch),
            Watch::Refused(_) => None,
        }
    }
}

impl ProcessWatch {
    /// Starts watching process `pid`. Returns [`BackendError::SourceGone`]
    /// when it doesn't run.
    pub(super) fn open(pid: u32) -> Result<Watch, BackendError> {
        // SAFETY: opens the process with the right to wait for it only; the
        // handle is owned by the watch.
        match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
            Ok(process) => {
                let watch = ProcessWatch(Handle::new(process));
                if watch.has_exited() {
                    Err(BackendError::SourceGone)
                } else {
                    Ok(Watch::Watching(watch))
                }
            }
            Err(error) if error.code() == NO_SUCH_PROCESS => Err(BackendError::SourceGone),
            Err(error) => Ok(Watch::Refused(error.code())),
        }
    }

    /// Whether the process exited.
    pub(super) fn has_exited(&self) -> bool {
        // SAFETY: a wait without timeout on a process handle this value owns.
        unsafe { WaitForSingleObject(self.0.raw(), 0) == WAIT_OBJECT_0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_executable_name_ends_at_its_nul() {
        let mut wide = [0u16; 260];
        for (unit, character) in wide.iter_mut().zip("player.exe".encode_utf16()) {
            *unit = character;
        }
        assert_eq!(executable_name(&wide), "player.exe");
        assert_eq!(executable_name(&[0x61, 0x62]), "ab");
    }
}
