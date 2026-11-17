//! Windows' default output per role for S469's seam: read through the
//! documented `IMMDeviceEnumerator::GetDefaultAudioEndpoint`, set through
//! the undocumented policy-config class, declared here from the facts in
//! docs/WINDOWS_AUDIO_ROUTING.md, "Interface facts and their sources".

use std::ffi::c_void;

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_ContainerId;
use windows::Win32::Foundation::ERROR_NOT_FOUND;
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, DEVICE_STATE_ACTIVE, EDataFlow, ERole, IAudioSessionManager2,
    IMMDevice, IMMDeviceEnumerator, eCapture, eConsole, eMultimedia, eRender,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, STGM_READ};
use windows::Win32::System::Variant::VT_CLSID;
use windows::core::{GUID, HRESULT, HSTRING, IUnknown, Interface, PCWSTR};

use super::com::Apartment;
use super::devices::{device_id, enumerator, properties};
use crate::audio::default_output::{Candidate, DefaultEndpoints, Role};
use crate::audio::outputs::output_kind;

/// `CPolicyConfigClient`.
const POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
/// `IPolicyConfig`; a class asked for `IUnknown` instead was seen to report
/// success without moving the default.
const POLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);
/// SetDefaultEndpoint's slot in `IPolicyConfig`'s table: the eleventh method
/// after `IUnknown`'s three.
const SET_DEFAULT_ENDPOINT_SLOT: usize = 13;

type SetDefaultEndpoint = unsafe extern "system" fn(*mut c_void, PCWSTR, ERole) -> HRESULT;

/// The real default output, for the controller's switch.
pub struct WasapiDefaults;

fn windows_role(role: Role) -> ERole {
    match role {
        Role::Console => eConsole,
        Role::Multimedia => eMultimedia,
    }
}

fn failure(call: &str, error: &windows::core::Error) -> String {
    format!("{call} failed (0x{:08X})", error.code().0)
}

impl DefaultEndpoints for WasapiDefaults {
    fn default_id(&mut self, role: Role) -> Result<Option<String>, String> {
        let _apartment = Apartment::enter().map_err(|error| error.to_string())?;
        let enumerator = enumerator().map_err(|error| error.to_string())?;
        // SAFETY: a call on a live enumerator.
        match unsafe { enumerator.GetDefaultAudioEndpoint(eRender, windows_role(role)) } {
            Ok(device) => device_id(&device).map(Some),
            Err(error) if error.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0) => Ok(None),
            Err(error) => Err(failure("GetDefaultAudioEndpoint", &error)),
        }
    }

    fn set_default(&mut self, id: &str, role: Role) -> Result<(), String> {
        let _apartment = Apartment::enter().map_err(|error| error.to_string())?;
        // SAFETY: creates an in-process COM object; COM is initialized above.
        let client: IUnknown = unsafe { CoCreateInstance(&POLICY_CONFIG_CLIENT, None, CLSCTX_ALL) }
            .map_err(|error| failure("CoCreateInstance(CPolicyConfigClient)", &error))?;
        let mut raw = std::ptr::null_mut();
        // SAFETY: asks the live object for the interface; on success `raw`
        // holds one reference, which `config` takes over and releases.
        unsafe { client.query(&POLICY_CONFIG, &mut raw) }
            .ok()
            .map_err(|error| failure("QueryInterface(IPolicyConfig)", &error))?;
        // SAFETY: as above.
        let config = unsafe { IUnknown::from_raw(raw) };
        let id = HSTRING::from(id);
        // SAFETY: a COM object's first field points to its method table;
        // slot 13 of IPolicyConfig's table is SetDefaultEndpoint, which
        // takes the object, a NUL-terminated ID that outlives the call and
        // a role.
        let result = unsafe {
            let table = *(config.as_raw() as *const *const *const c_void);
            let call: SetDefaultEndpoint =
                std::mem::transmute(*table.add(SET_DEFAULT_ENDPOINT_SLOT));
            call(config.as_raw(), PCWSTR(id.as_ptr()), windows_role(role))
        };
        result
            .ok()
            .map_err(|error| failure("IPolicyConfig::SetDefaultEndpoint", &error))
    }

    fn endpoints(&mut self) -> Result<Vec<Candidate>, String> {
        let _apartment = Apartment::enter().map_err(|error| error.to_string())?;
        let enumerator = enumerator().map_err(|error| error.to_string())?;
        let recorded: Vec<GUID> = active_devices(&enumerator, eCapture)?
            .iter()
            .filter(|device| records(device))
            .filter_map(container)
            .collect();
        Ok(active_devices(&enumerator, eRender)?
            .iter()
            .filter_map(|device| {
                let id = device_id(device).ok()?;
                let properties = properties(device).ok()?;
                let kind = output_kind(
                    properties.form_factor,
                    properties.enumerator.as_deref().unwrap_or_default(),
                    properties.display_port,
                );
                let listened = container(device).is_some_and(|guid| recorded.contains(&guid));
                Some(Candidate { id, kind, listened })
            })
            .collect())
    }
}

/// The active endpoints of `flow`; one that fails is left out.
fn active_devices(
    enumerator: &IMMDeviceEnumerator,
    flow: EDataFlow,
) -> Result<Vec<IMMDevice>, String> {
    // SAFETY: calls on a live enumerator and its collection, indices below
    // the count.
    unsafe {
        let devices = enumerator
            .EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)
            .map_err(|error| failure("EnumAudioEndpoints", &error))?;
        let count = devices
            .GetCount()
            .map_err(|error| failure("IMMDeviceCollection::GetCount", &error))?;
        Ok((0..count)
            .filter_map(|index| devices.Item(index).ok())
            .collect())
    }
}

/// Whether an app records from the capture endpoint now.
fn records(device: &IMMDevice) -> bool {
    // SAFETY: calls on live COM objects this function owns; only reads.
    unsafe {
        let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
            return false;
        };
        let Ok(sessions) = manager.GetSessionEnumerator() else {
            return false;
        };
        let count = sessions.GetCount().unwrap_or(0);
        (0..count).any(|index| {
            sessions.GetSession(index).is_ok_and(|session| {
                session
                    .GetState()
                    .is_ok_and(|state| state == AudioSessionStateActive)
            })
        })
    }
}

/// The device container the endpoint belongs to: a driver's render and
/// capture endpoints share it.
fn container(device: &IMMDevice) -> Option<GUID> {
    // SAFETY: reads one property into a PROPVARIANT this function owns and
    // clears; its GUID is read only when the variant says it holds one.
    unsafe {
        let store = device.OpenPropertyStore(STGM_READ).ok()?;
        let mut value = store.GetValue(&PKEY_Device_ContainerId).ok()?;
        let guid = (value.Anonymous.Anonymous.vt == VT_CLSID)
            .then(|| value.Anonymous.Anonymous.Anonymous.puuid.as_ref().copied())
            .flatten();
        let _ = PropVariantClear(&mut value);
        guid
    }
}
