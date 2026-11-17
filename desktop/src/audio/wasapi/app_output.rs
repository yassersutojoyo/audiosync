//! One app's own output per role for S471's seam, through the undocumented
//! per-app policy factory, declared here from the interface facts in
//! docs/WINDOWS_AUDIO_ROUTING.md, "Moving one app's output".

use std::ffi::c_void;

use windows::Win32::Media::Audio::{ERole, eConsole, eMultimedia, eRender};
use windows::Win32::System::WinRT::RoGetActivationFactory;
use windows::core::{GUID, HRESULT, HSTRING, IInspectable, Interface};

use super::com::Apartment;
use crate::audio::app_output::{AppEndpoints, Variant};
use crate::audio::default_output::Role;

const CLASS: &str = "Windows.Media.Internal.AudioPolicyConfig";
const EARLIER_IID: GUID = GUID::from_u128(0x2a59116d_6c4f_45e0_a74f_707e3fef9258);
const LATER_IID: GUID = GUID::from_u128(0xab3d4648_e242_459f_b02f_541c70306324);
/// Slots counted from QueryInterface; both variants share them. The
/// clear-every-app method in the next slot is never called.
const SET_SLOT: usize = 25;
const GET_SLOT: usize = 26;

type SetPersisted = unsafe extern "system" fn(*mut c_void, u32, i32, ERole, *mut c_void) -> HRESULT;
type GetPersisted =
    unsafe extern "system" fn(*mut c_void, u32, i32, ERole, *mut *mut c_void) -> HRESULT;

/// The real per-app outputs, for the controller's redirect.
pub struct WasapiAppOutputs(pub Variant);

fn windows_role(role: Role) -> ERole {
    match role {
        Role::Console => eConsole,
        Role::Multimedia => eMultimedia,
    }
}

impl WasapiAppOutputs {
    /// The factory, or why it can't be had: a build that dropped or changed
    /// it answers here, and the redirect falls back.
    fn factory(&self) -> Result<IInspectable, String> {
        let iid = match self.0 {
            Variant::Earlier => EARLIER_IID,
            Variant::Later => LATER_IID,
        };
        // SAFETY: activates a factory and asks it for the variant's IID; on
        // success `raw` holds one reference, which the result takes over.
        unsafe {
            let base: IInspectable =
                RoGetActivationFactory(&HSTRING::from(CLASS)).map_err(|error| {
                    format!("RoGetActivationFactory failed (0x{:08X})", error.code().0)
                })?;
            let mut raw = std::ptr::null_mut();
            base.query(&iid, &mut raw).ok().map_err(|error| {
                format!(
                    "the per-app interface is missing (0x{:08X})",
                    error.code().0
                )
            })?;
            Ok(IInspectable::from_raw(raw))
        }
    }

    /// Checks that the interface can be had on this build.
    pub fn probe(&self) -> Result<(), String> {
        let _apartment = Apartment::enter().map_err(|error| error.to_string())?;
        self.factory().map(|_| ())
    }
}

/// The method in `slot` of `object`'s table.
///
/// # Safety
/// `object` must be a live interface whose table has `slot`.
unsafe fn method(object: &IInspectable, slot: usize) -> *const c_void {
    // SAFETY: a COM object's first field points to its method table.
    unsafe {
        let table = *(object.as_raw() as *const *const *const c_void);
        *table.add(slot)
    }
}

impl AppEndpoints for WasapiAppOutputs {
    fn app_default(&mut self, pid: u32, role: Role) -> Result<String, String> {
        let _apartment = Apartment::enter().map_err(|error| error.to_string())?;
        let factory = self.factory()?;
        let mut out: *mut c_void = std::ptr::null_mut();
        // SAFETY: slot 26 reads one app's output into an HSTRING the caller
        // owns, which `HSTRING` takes over and frees.
        let result = unsafe {
            let call: GetPersisted = std::mem::transmute(method(&factory, GET_SLOT));
            call(
                factory.as_raw(),
                pid,
                eRender.0,
                windows_role(role),
                &mut out,
            )
        };
        result.ok().map_err(|error| {
            format!("reading the app's output failed (0x{:08X})", error.code().0)
        })?;
        // SAFETY: `out` is null or an HSTRING handed over by the call.
        let device: HSTRING = unsafe { std::mem::transmute::<*mut c_void, HSTRING>(out) };
        Ok(device.to_string())
    }

    fn set_app_default(&mut self, pid: u32, role: Role, device: &str) -> Result<(), String> {
        let _apartment = Apartment::enter().map_err(|error| error.to_string())?;
        let factory = self.factory()?;
        let device = HSTRING::from(device);
        // SAFETY: slot 25 sets one app's output; the HSTRING is passed by
        // value and stays alive and owned here through the call.
        let result = unsafe {
            let call: SetPersisted = std::mem::transmute(method(&factory, SET_SLOT));
            call(
                factory.as_raw(),
                pid,
                eRender.0,
                windows_role(role),
                std::mem::transmute_copy(&device),
            )
        };
        result
            .ok()
            .map_err(|error| format!("setting the app's output failed (0x{:08X})", error.code().0))
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::Media::Audio::{
        AUDCLNT_SHAREMODE_SHARED, IAudioClient, IAudioRenderClient,
    };
    use windows::Win32::System::Com::CLSCTX_ALL;

    use super::*;
    use crate::audio::app_output::{LATER_INTERFACE_BUILD, render_interface};
    use crate::audio::default_output::{DefaultEndpoints, silent_endpoint};
    use crate::audio::wasapi::WasapiDefaults;
    use crate::audio::wasapi::devices::enumerator;

    /// Hardware check of the interface facts on this PC, on the test's own
    /// process only: it opens a stream of silence on the default output so
    /// it has an audio session, reads its own output, moves itself to the
    /// silent endpoint, reads it back and puts itself back on "Default".
    /// `cargo test -- --ignored the_per_app_output_reads_back`.
    #[test]
    #[ignore = "changes this test process's own output on the real PC"]
    fn the_per_app_output_reads_back_on_this_pc() {
        let build = crate::platform::windows_version()
            .expect("the OS version")
            .build;
        assert!(
            build >= LATER_INTERFACE_BUILD,
            "only the later variant is checked here"
        );
        let _apartment = Apartment::enter().expect("COM");
        // SAFETY: opens and starts a shared stream this test owns; its buffer
        // is released as silence, so nothing is heard.
        let _client = unsafe {
            let device = enumerator()
                .expect("enumerator")
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .expect("default output");
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).expect("client");
            let format = client.GetMixFormat().expect("mix format");
            client
                .Initialize(AUDCLNT_SHAREMODE_SHARED, 0, 1_000_000, 0, format, None)
                .expect("initialize");
            let render: IAudioRenderClient = client.GetService().expect("render client");
            let frames = client.GetBufferSize().expect("buffer size");
            render.GetBuffer(frames).expect("buffer");
            render
                .ReleaseBuffer(
                    frames,
                    windows::Win32::Media::Audio::AUDCLNT_BUFFERFLAGS_SILENT.0 as u32,
                )
                .expect("release");
            client.Start().expect("start");
            client
        };
        let listed = WasapiDefaults.endpoints().expect("endpoints");
        let silent = render_interface(silent_endpoint(&listed, &[]).expect("a silent endpoint"));
        let mut outputs = WasapiAppOutputs(Variant::Later);
        let pid = std::process::id();
        let before = outputs.app_default(pid, Role::Console).expect("read");
        eprintln!("before: {before:?}");
        outputs
            .set_app_default(pid, Role::Console, &silent)
            .expect("set");
        let moved = outputs.app_default(pid, Role::Console).expect("read back");
        outputs
            .set_app_default(pid, Role::Console, "")
            .expect("clear");
        let cleared = outputs
            .app_default(pid, Role::Console)
            .expect("read cleared");
        eprintln!("moved: {moved:?}\ncleared: {cleared:?}");
        assert_eq!(moved, silent);
        assert_eq!(cleared, "");
    }
}
