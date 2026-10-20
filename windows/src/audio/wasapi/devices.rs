//! Render endpoints: listing them, and noticing when they change.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use windows::Win32::Devices::FunctionDiscovery::{
    PKEY_Device_EnumeratorName, PKEY_Device_FriendlyName,
};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    DEVICE_STATE, DEVICE_STATE_ACTIVE, EDataFlow, ERole, IMMDevice, IMMDeviceEnumerator,
    IMMNotificationClient, IMMNotificationClient_Impl, MMDeviceEnumerator,
    PKEY_AudioEndpoint_FormFactor, PKEY_AudioEndpoint_JackSubType, eConsole, eRender,
};
use windows::Win32::Media::KernelStreaming::KSNODETYPE_DISPLAYPORT_INTERFACE;
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance, STGM_READ};
use windows::Win32::System::Threading::{INFINITE, WaitForSingleObject};
use windows::Win32::System::Variant::{VT_LPWSTR, VT_UI4};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
use windows::core::{GUID, PCWSTR};
use windows_core::implement;

use super::com::{Apartment, Event, Ready, spawn_ready, system_error, take_com_string};
use crate::audio::backend::BackendError;
use crate::audio::outputs::{EndpointProperties, EndpointReading, Listing, list_endpoints};

/// The device enumerator. COM must be initialized on the calling thread.
pub(super) fn enumerator() -> Result<IMMDeviceEnumerator, BackendError> {
    // SAFETY: creates an in-process COM object; the caller initialized COM.
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .map_err(|error| system_error("CoCreateInstance(MMDeviceEnumerator)", &error))
}

/// The active render endpoints (Bluetooth ones included while connected),
/// by ID, without the Bluetooth hands-free ones, and notes for the log on
/// what was left out and why (see [`list_endpoints`]).
///
/// Windows can leave a hands-free endpoint out of the enumeration itself
/// (seen with the one a PC gets for a phone whose calls it handles: active,
/// and found by its ID, yet not enumerated), but a connected headset's
/// hands-free endpoint is enumerated next to its stereo one.
pub(super) fn render_endpoints() -> Result<Listing, BackendError> {
    let _apartment = Apartment::enter()?;
    let enumerator = enumerator()?;
    // SAFETY: calls on a live enumerator; no default output is an error,
    // which leaves `default_id` empty.
    let default_id = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
        .ok()
        .and_then(|device| device_id(&device).ok());
    // SAFETY: as above.
    let devices = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }
        .map_err(|error| system_error("IMMDeviceEnumerator::EnumAudioEndpoints", &error))?;
    // SAFETY: a call on the live collection.
    let count = unsafe { devices.GetCount() }
        .map_err(|error| system_error("IMMDeviceCollection::GetCount", &error))?;
    let readings = (0..count)
        .map(|index| {
            // SAFETY: an index below the count; a device that fails is
            // noted and left out.
            match unsafe { devices.Item(index) } {
                Ok(device) => EndpointReading {
                    index,
                    id: device_id(&device),
                    properties: properties(&device),
                },
                Err(error) => EndpointReading {
                    index,
                    id: Err(failure("IMMDeviceCollection::Item", &error)),
                    properties: Ok(EndpointProperties::default()),
                },
            }
        })
        .collect();
    Ok(list_endpoints(readings, default_id.as_deref()))
}

/// The endpoint ID of `device`, or why it couldn't be read.
pub(super) fn device_id(device: &IMMDevice) -> Result<String, String> {
    // SAFETY: GetId hands over a COM-allocated string, which
    // take_com_string copies and frees.
    let text = unsafe { device.GetId() }.map_err(|error| failure("IMMDevice::GetId", &error))?;
    // SAFETY: as above.
    unsafe { take_com_string(text) }.ok_or_else(|| "IMMDevice::GetId gave no ID".to_owned())
}

/// The properties of `device` the listing uses, or why its property store
/// couldn't be opened. A property that can't be read is left out.
pub(super) fn properties(device: &IMMDevice) -> Result<EndpointProperties, String> {
    // SAFETY: opens the endpoint's own property store for reading.
    let store = unsafe { device.OpenPropertyStore(STGM_READ) }
        .map_err(|error| failure("IMMDevice::OpenPropertyStore", &error))?;
    Ok(EndpointProperties {
        name: string_property(&store, &PKEY_Device_FriendlyName),
        form_factor: u32_property(&store, &PKEY_AudioEndpoint_FormFactor),
        enumerator: string_property(&store, &PKEY_Device_EnumeratorName),
        display_port: string_property(&store, &PKEY_AudioEndpoint_JackSubType)
            .and_then(|text| GUID::try_from(text.trim_matches(['{', '}'])).ok())
            == Some(KSNODETYPE_DISPLAYPORT_INTERFACE),
    })
}

/// A failed call, for a listing note.
fn failure(call: &str, error: &windows::core::Error) -> String {
    // `{:08X}` of a negative i32 prints its two's complement, the way
    // Windows writes HRESULTs.
    format!("{call} failed (0x{:08X})", error.code().0)
}

/// A string property of `store`, if it holds one under `key`.
fn string_property(store: &IPropertyStore, key: &PROPERTYKEY) -> Option<String> {
    // SAFETY: reads one property into a PROPVARIANT this function owns and
    // clears; its string is read only when the variant says it holds one.
    unsafe {
        let mut value = store.GetValue(key).ok()?;
        let text = if value.Anonymous.Anonymous.vt == VT_LPWSTR {
            value.Anonymous.Anonymous.Anonymous.pwszVal.to_string().ok()
        } else {
            None
        };
        let _ = PropVariantClear(&mut value);
        text
    }
}

/// An unsigned 32-bit property of `store`, if it holds one under `key`.
fn u32_property(store: &IPropertyStore, key: &PROPERTYKEY) -> Option<u32> {
    // SAFETY: reads one property into a PROPVARIANT this function owns and
    // clears; its number is read only when the variant says it holds one.
    unsafe {
        let mut value = store.GetValue(key).ok()?;
        let number = (value.Anonymous.Anonymous.vt == VT_UI4)
            .then(|| value.Anonymous.Anonymous.Anonymous.ulVal);
        let _ = PropVariantClear(&mut value);
        number
    }
}

/// Watches for render endpoints being added, removed, enabled, disabled,
/// renamed or made the default. A thread of its own holds the registration,
/// so the watcher can be used from any thread.
pub(super) struct DeviceWatcher {
    changed: Arc<AtomicBool>,
    stop: Arc<Event>,
    thread: Option<JoinHandle<()>>,
}

impl DeviceWatcher {
    pub(super) fn start() -> Result<DeviceWatcher, BackendError> {
        DeviceWatcher::start_with(watch)
    }

    /// A watcher whose thread runs `body`, which sets the flag it's given
    /// when endpoints change and ends once the event it's given is set.
    fn start_with(
        body: impl FnOnce(&Arc<AtomicBool>, &Event, Ready) + Send + 'static,
    ) -> Result<DeviceWatcher, BackendError> {
        let changed = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(Event::manual_reset()?);
        let (thread, ()) = {
            let changed = Arc::clone(&changed);
            let stop = Arc::clone(&stop);
            spawn_ready("audiosync-devices", move |ready| {
                body(&changed, &stop, ready);
            })?
        };
        Ok(DeviceWatcher {
            changed,
            stop,
            thread: Some(thread),
        })
    }

    /// Whether endpoints changed since the last call.
    pub(super) fn take_changed(&self) -> bool {
        self.changed.swap(false, Ordering::AcqRel)
    }
}

impl Drop for DeviceWatcher {
    fn drop(&mut self) {
        self.stop.set();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The watcher thread: registers, waits for the stop request, unregisters.
fn watch(changed: &Arc<AtomicBool>, stop: &Event, ready: Ready) {
    let _apartment = match Apartment::enter() {
        Ok(apartment) => apartment,
        Err(error) => return ready.fail(error),
    };
    let registration = match Registration::new(changed) {
        Ok(registration) => registration,
        Err(error) => return ready.fail(error),
    };
    if !ready.ok(()) {
        // Nobody waits for this watcher anymore, and nothing would stop it.
        return;
    }
    // SAFETY: waits on an event that outlives this thread's use of it.
    unsafe { WaitForSingleObject(stop.raw(), INFINITE) };
    drop(registration);
}

/// A registered listener, unregistered on drop.
struct Registration {
    enumerator: IMMDeviceEnumerator,
    listener: IMMNotificationClient,
}

impl Registration {
    fn new(changed: &Arc<AtomicBool>) -> Result<Registration, BackendError> {
        let enumerator = enumerator()?;
        let listener: IMMNotificationClient = DeviceListener {
            changed: Arc::clone(changed),
        }
        .into();
        // SAFETY: the listener stays alive and registered until this value
        // unregisters it on drop.
        unsafe { enumerator.RegisterEndpointNotificationCallback(&listener) }.map_err(|error| {
            system_error(
                "IMMDeviceEnumerator::RegisterEndpointNotificationCallback",
                &error,
            )
        })?;
        Ok(Registration {
            enumerator,
            listener,
        })
    }
}

impl Drop for Registration {
    fn drop(&mut self) {
        // SAFETY: unregisters the listener registered on this enumerator.
        let _ = unsafe {
            self.enumerator
                .UnregisterEndpointNotificationCallback(&self.listener)
        };
    }
}

/// Raises a flag when render endpoints change. Windows calls it on its own
/// threads and asks it not to wait or call back into the enumerator, so it
/// only stores the flag. `#[implement]` objects are agile (they answer
/// `IAgileObject` and use the free-threaded marshaler), so the calls reach it
/// directly.
#[implement(IMMNotificationClient)]
struct DeviceListener {
    changed: Arc<AtomicBool>,
}

impl DeviceListener {
    fn note(&self) {
        self.changed.store(true, Ordering::Release);
    }
}

impl IMMNotificationClient_Impl for DeviceListener_Impl {
    fn OnDeviceStateChanged(
        &self,
        _device_id: &PCWSTR,
        _new_state: DEVICE_STATE,
    ) -> windows_core::Result<()> {
        self.note();
        Ok(())
    }

    fn OnDeviceAdded(&self, _device_id: &PCWSTR) -> windows_core::Result<()> {
        self.note();
        Ok(())
    }

    fn OnDeviceRemoved(&self, _device_id: &PCWSTR) -> windows_core::Result<()> {
        self.note();
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        _role: ERole,
        _default_device_id: &PCWSTR,
    ) -> windows_core::Result<()> {
        if flow == eRender {
            self.note();
        }
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _device_id: &PCWSTR,
        key: &PROPERTYKEY,
    ) -> windows_core::Result<()> {
        if *key == PKEY_Device_FriendlyName {
            self.note();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::{Duration, Instant};

    use super::*;

    /// How long the fake Windows audio hangs before it answers again.
    const HANG: Duration = Duration::from_secs(3);

    #[test]
    fn a_device_thread_that_hangs_unregistering_its_listener_doesnt_hold_quitting() {
        let (answer, hung) = mpsc::channel::<()>();
        let (unregistered, done) = mpsc::channel();
        let watcher = DeviceWatcher::start_with(move |_changed, stop, ready| {
            if ready.ok(()) {
                // SAFETY: waits on an event that outlives this thread's use
                // of it.
                unsafe { WaitForSingleObject(stop.raw(), INFINITE) };
                // Unregistering the listener hangs until Windows audio
                // answers.
                let _ = hung.recv();
                let _ = unregistered.send(());
            }
        })
        .expect("it starts");
        let answers = thread::spawn(move || {
            thread::sleep(HANG);
            drop(answer);
        });
        let started = Instant::now();
        drop(watcher);
        let waited = started.elapsed();
        assert!(waited < HANG / 2, "the drop waited {waited:?}");
        // Once Windows audio answers, the thread still unregisters.
        assert_eq!(done.recv_timeout(HANG * 3), Ok(()));
        answers.join().expect("the answer comes");
    }
}
