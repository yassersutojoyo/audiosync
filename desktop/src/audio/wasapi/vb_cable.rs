//! VB-CABLE on Windows: its device found by hardware ID through the
//! configuration manager, and the [`Platform`] the install of
//! `crate::audio::silent_output` runs on.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_GETIDLIST_FILTER_ENUMERATOR, CM_Get_DevNode_PropertyW, CM_Get_Device_ID_List_SizeW,
    CM_Get_Device_ID_ListW, CM_Get_Parent, CM_LOCATE_DEVNODE_NORMAL, CM_LOCATE_DEVNODE_PHANTOM,
    CM_Locate_DevNodeW, CR_SUCCESS,
};
use windows::Win32::Devices::FunctionDiscovery::PKEY_DeviceInterface_FriendlyName;
use windows::Win32::Devices::Properties::{DEVPKEY_Device_HardwareIds, DEVPROPTYPE};
use windows::Win32::Foundation::{CloseHandle, ERROR_CANCELLED, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    DEVICE_STATE_ACTIVE, EDataFlow, ERole, eCapture, eCommunications, eConsole, eMultimedia,
    eRender,
};
use windows::Win32::System::Com::STGM_READ;
use windows::Win32::System::Threading::WaitForSingleObject;
use windows::core::{HSTRING, PCWSTR};

use super::com::Apartment;
use super::default_output::set_default_endpoint;
use super::devices::{device_id, enumerator, string_property};
use crate::audio::silent_output::{
    FOLDER_NAME, HARDWARE_ID, Kept, Launch, Platform, Role, SLOTS, Setup, Slot, fetch_pack,
    is_vb_cable, unpack_into,
};
use crate::updater::fetch::WinHttp;

/// The configuration manager's string list `ids`, split at its NULs.
fn split_list(ids: &[u16]) -> Vec<String> {
    ids.split(|unit| *unit == 0)
        .filter(|id| !id.is_empty())
        .map(String::from_utf16_lossy)
        .collect()
}

/// The device node of the device instance `id`, also one not present now
/// with `phantom`.
fn locate(id: &str, phantom: bool) -> Option<u32> {
    let mut node = 0u32;
    let flags = if phantom {
        CM_LOCATE_DEVNODE_PHANTOM
    } else {
        CM_LOCATE_DEVNODE_NORMAL
    };
    // SAFETY: the ID is NUL-terminated and outlives the call.
    let result = unsafe { CM_Locate_DevNodeW(&mut node, &HSTRING::from(id), flags) };
    (result == CR_SUCCESS).then_some(node)
}

/// A device node's hardware IDs; none when they can't be read.
fn hardware_ids(node: u32) -> Vec<String> {
    let mut kind = DEVPROPTYPE::default();
    let mut size = 0u32;
    // SAFETY: a size query: no buffer, `size` receives the bytes needed.
    let _ = unsafe {
        CM_Get_DevNode_PropertyW(
            node,
            &DEVPKEY_Device_HardwareIds,
            &mut kind,
            None,
            &mut size,
            0,
        )
    };
    if size == 0 {
        return Vec::new();
    }
    let mut buffer = vec![0u16; size.div_ceil(2) as usize];
    // SAFETY: the buffer holds `size` bytes.
    let result = unsafe {
        CM_Get_DevNode_PropertyW(
            node,
            &DEVPKEY_Device_HardwareIds,
            &mut kind,
            Some(buffer.as_mut_ptr().cast()),
            &mut size,
            0,
        )
    };
    if result != CR_SUCCESS {
        return Vec::new();
    }
    split_list(&buffer)
}

/// Whether the endpoint `endpoint_id` belongs to VB-CABLE: the hardware IDs
/// of its device, the parent of the endpoint's own `SWD\MMDEVAPI` node, or
/// its device's name when they can't be read.
pub(super) fn endpoint_is_vb_cable(endpoint_id: &str, device_name: Option<&str>) -> bool {
    let ids = locate(&format!("SWD\\MMDEVAPI\\{endpoint_id}"), false)
        .and_then(|endpoint| {
            let mut parent = 0u32;
            // SAFETY: a node the configuration manager returned.
            let result = unsafe { CM_Get_Parent(&mut parent, endpoint, 0) };
            (result == CR_SUCCESS).then_some(parent)
        })
        .map(hardware_ids)
        .unwrap_or_default();
    is_vb_cable(&ids, device_name)
}

/// Whether a VB-CABLE device exists in any state: present, disabled or
/// installed without its endpoints yet.
pub fn cable_present() -> bool {
    let filter = HSTRING::from("ROOT");
    let mut size = 0u32;
    // SAFETY: `size` receives the list's length in characters.
    let result =
        unsafe { CM_Get_Device_ID_List_SizeW(&mut size, &filter, CM_GETIDLIST_FILTER_ENUMERATOR) };
    if result != CR_SUCCESS || size == 0 {
        return false;
    }
    let mut list = vec![0u16; size as usize];
    // SAFETY: the buffer holds the length the size query gave.
    let result =
        unsafe { CM_Get_Device_ID_ListW(&filter, &mut list, CM_GETIDLIST_FILTER_ENUMERATOR) };
    if result != CR_SUCCESS {
        return false;
    }
    split_list(&list)
        .iter()
        .filter(|id| id.to_ascii_uppercase().starts_with("ROOT\\"))
        .filter_map(|id| locate(id, true))
        .any(|node| {
            hardware_ids(node)
                .iter()
                .any(|id| id.eq_ignore_ascii_case(HARDWARE_ID))
        })
}

/// The installer's step (VC25): the install Fix It runs, extracted next to
/// this exe; 0 when VB-CABLE was added. The folder stays only while a cable
/// device exists, so the uninstaller removes no cable it didn't add (VC53).
pub fn add_for_installer() -> i32 {
    let log = crate::debug_log::DebugLog::appending_from_env();
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let platform = WindowsPlatform {
        user_agent: crate::updater::fetch::user_agent(
            "Windows",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::ARCH,
        ),
        data_folder: beside.clone(),
        kept: kept_file(),
    };
    let result = crate::audio::silent_output::install(&platform, &mut |line| log.log(|| line));
    log.log(|| format!("silent output: the installer's step: {result:?}"));
    if let Some(folder) = beside.map(|beside| beside.join(FOLDER_NAME))
        && folder.exists()
        && !cable_present()
    {
        let removed = std::fs::remove_dir_all(&folder);
        log.log(|| format!("silent output: {} removed: {removed:?}", folder.display()));
    }
    log.flush(crate::LOG_FLUSH_AT_EXIT);
    i32::from(result.is_err())
}

fn flow(slot: Slot) -> EDataFlow {
    if slot.capture { eCapture } else { eRender }
}

fn role(slot: Slot) -> ERole {
    match slot.role {
        Role::Console => eConsole,
        Role::Multimedia => eMultimedia,
        Role::Communications => eCommunications,
    }
}

/// The setup VB-Audio's installer runs as, held by its process handle.
struct Running(HANDLE);

// SAFETY: a process handle may be waited on and closed from any thread.
unsafe impl Send for Running {}

impl Setup for Running {
    fn exited(&mut self) -> bool {
        // SAFETY: the handle is open until drop; a zero wait only polls.
        unsafe { WaitForSingleObject(self.0, 0) == WAIT_OBJECT_0 }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        // SAFETY: closed once, after its last use.
        let _ = unsafe { CloseHandle(self.0) };
    }
}

/// The real Windows under the install.
pub struct WindowsPlatform {
    pub user_agent: String,
    /// The app's data folder; `None` leaves nowhere to extract to.
    pub data_folder: Option<PathBuf>,
    /// The file that keeps VC30's defaults (VC42); `None` keeps none.
    pub kept: Option<PathBuf>,
}

const KEPT_FILE_NAME: &str = "silent-output-defaults.json";

/// The file in the user's data folder that keeps VC30's defaults (VC42).
pub fn kept_file() -> Option<PathBuf> {
    crate::data_folder::folder().map(|folder| folder.join(KEPT_FILE_NAME))
}

/// At launch and in S474's guard (VC42).
pub fn restore_kept(log: &crate::debug_log::DebugLog) {
    let platform = WindowsPlatform {
        user_agent: String::new(),
        data_folder: None,
        kept: kept_file(),
    };
    crate::audio::silent_output::restore_kept(&platform, &mut |line| log.log(|| line));
}

/// Whether VC30's defaults are kept on disk.
pub fn kept_on_disk() -> bool {
    kept_file().is_some_and(|path| path.exists())
}

impl Platform for WindowsPlatform {
    fn arm64(&self) -> bool {
        crate::platform::native_machine() == Some("aarch64")
    }

    fn cable_present(&self) -> bool {
        cable_present()
    }

    fn cable_endpoints(&self) -> Vec<String> {
        let Ok(_apartment) = Apartment::enter() else {
            return Vec::new();
        };
        let Ok(enumerator) = enumerator() else {
            return Vec::new();
        };
        let mut found = Vec::new();
        for flow in [eRender, eCapture] {
            // SAFETY: calls on a live enumerator and its collection, indices
            // below the count.
            unsafe {
                let Ok(devices) = enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE) else {
                    continue;
                };
                let count = devices.GetCount().unwrap_or(0);
                for index in 0..count {
                    let Ok(device) = devices.Item(index) else {
                        continue;
                    };
                    let Ok(id) = device_id(&device) else {
                        continue;
                    };
                    let name = device.OpenPropertyStore(STGM_READ).ok().and_then(|store| {
                        string_property(&store, &PKEY_DeviceInterface_FriendlyName)
                    });
                    if endpoint_is_vb_cable(&id, name.as_deref()) {
                        found.push(id);
                    }
                }
            }
        }
        found
    }

    fn endpoint_active(&self, id: &str) -> bool {
        let Ok(_apartment) = Apartment::enter() else {
            return false;
        };
        let Ok(enumerator) = enumerator() else {
            return false;
        };
        // SAFETY: calls on a live enumerator and the device it returned.
        unsafe {
            enumerator
                .GetDevice(&HSTRING::from(id))
                .and_then(|device| device.GetState())
                .is_ok_and(|state| state == DEVICE_STATE_ACTIVE)
        }
    }

    fn defaults(&self) -> Vec<Option<String>> {
        let enumerator = Apartment::enter()
            .ok()
            .and_then(|apartment| enumerator().ok().map(|enumerator| (apartment, enumerator)));
        SLOTS
            .iter()
            .map(|slot| {
                let (_, enumerator) = enumerator.as_ref()?;
                // SAFETY: a call on a live enumerator; no default is an error.
                let device =
                    unsafe { enumerator.GetDefaultAudioEndpoint(flow(*slot), role(*slot)) }.ok()?;
                device_id(&device).ok()
            })
            .collect()
    }

    fn set_default(&self, slot: Slot, id: &str) -> Result<(), String> {
        set_default_endpoint(id, role(slot))
    }

    fn keep_before(&self, kept: Option<&Kept>) -> Result<(), String> {
        let Some(path) = &self.kept else {
            return Err("there is no data folder".to_owned());
        };
        let result = match kept {
            Some(kept) => serde_json::to_vec(kept)
                .map_err(std::io::Error::other)
                .and_then(|bytes| crate::data_folder::write_atomically(path, &bytes)),
            None => match std::fs::remove_file(path) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                other => other,
            },
        };
        result.map_err(|error| error.to_string())
    }

    fn kept_before(&self) -> Result<Option<Kept>, String> {
        let Some(path) = &self.kept else {
            return Ok(None);
        };
        match std::fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|error| error.to_string()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.to_string()),
        }
    }

    fn download(&self) -> Result<Vec<u8>, String> {
        fetch_pack(&WinHttp {
            user_agent: self.user_agent.clone(),
            plain_loopback: false,
        })
    }

    fn unpack(&self, pack: &[u8]) -> Result<(PathBuf, Vec<std::fs::File>), String> {
        let folder = self
            .data_folder
            .as_ref()
            .ok_or_else(|| "no data folder".to_owned())?
            .join(FOLDER_NAME);
        let held = unpack_into(pack, &folder)?;
        Ok((folder, held))
    }

    fn launch(&self, setup: &Path, switches: &str) -> Result<Box<dyn Setup>, Launch> {
        use windows::Win32::System::Com::{
            COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoInitializeEx, CoUninitialize,
        };
        use windows::Win32::UI::Shell::{
            SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
        };
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let wide = |text: &OsStr| text.encode_wide().chain([0]).collect::<Vec<u16>>();
        let file = wide(setup.as_os_str());
        let folder = wide(setup.parent().unwrap_or(setup).as_os_str());
        let verb = wide(OsStr::new("runas"));
        let parameters = wide(OsStr::new(switches));
        let mut info = SHELLEXECUTEINFOW {
            cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
            lpVerb: PCWSTR(verb.as_ptr()),
            lpFile: PCWSTR(file.as_ptr()),
            lpParameters: PCWSTR(parameters.as_ptr()),
            lpDirectory: PCWSTR(folder.as_ptr()),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        // ShellExecuteExW can hand the verb to a shell extension, which needs COM.
        // SAFETY: balanced below on success; a thread already in COM keeps its mode.
        let com =
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) };
        // SAFETY: every string outlives the call and is NUL-terminated.
        let executed = unsafe { ShellExecuteExW(&mut info) };
        if com.is_ok() {
            // SAFETY: pairs the successful CoInitializeEx above.
            unsafe { CoUninitialize() };
        }
        match executed {
            Err(error) if error.code() == ERROR_CANCELLED.to_hresult() => Err(Launch::Refused),
            Err(error) => Err(Launch::Failed(format!("ShellExecuteExW: {error}"))),
            Ok(()) if info.hProcess.is_invalid() => {
                Err(Launch::Failed("no process handle".to_owned()))
            }
            Ok(()) => Ok(Box::new(Running(info.hProcess))),
        }
    }

    fn now(&self) -> Instant {
        Instant::now()
    }

    fn pause(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // covers: VC45
    #[test]
    fn the_configuration_managers_lists_split_at_their_nuls() {
        let list: Vec<u16> = "ROOT\\MEDIA\\0000\0VBAudioVACWDM\0\0"
            .encode_utf16()
            .collect();
        assert_eq!(
            split_list(&list),
            vec!["ROOT\\MEDIA\\0000", "VBAudioVACWDM"]
        );
        assert!(split_list(&[0, 0]).is_empty());
    }
}
