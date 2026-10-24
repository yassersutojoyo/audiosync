//! The running processes, the audio sessions they own, and watching a process
//! for its exit.

use windows::Win32::Foundation::{ERROR_INVALID_PARAMETER, FILETIME, S_OK, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AudioSessionStateExpired, DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2,
    eRender,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    WaitForSingleObject,
};
use windows::core::{HRESULT, Interface};

use super::com::{Apartment, Handle, system_error};
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

impl ProcessWatch {
    /// Starts watching process `pid`. Returns [`BackendError::SourceGone`]
    /// when it doesn't run, and `None` when it runs but can't be opened (an
    /// app running as administrator, say), so its exit goes unnoticed.
    pub(super) fn open(pid: u32) -> Result<Option<ProcessWatch>, BackendError> {
        // SAFETY: opens the process with the right to wait for it only; the
        // handle is owned by the watch.
        match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
            Ok(process) => {
                let watch = ProcessWatch(Handle::new(process));
                if watch.has_exited() {
                    Err(BackendError::SourceGone)
                } else {
                    Ok(Some(watch))
                }
            }
            Err(error) if error.code() == NO_SUCH_PROCESS => Err(BackendError::SourceGone),
            Err(_) => Ok(None),
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
