//! The snapshot the double-audio check reads: each active render endpoint's
//! name and kind, whether it is the default output, its mute and master
//! volume (`IAudioEndpointVolume`), and the processes with an active audio
//! session on it (`IAudioSessionManager2`). It only reads: no mute, volume or
//! default output is ever changed here.

use windows::Win32::Foundation::S_OK;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    AudioSessionStateActive, DEVICE_STATE_ACTIVE, IAudioSessionControl2, IAudioSessionManager2,
    IMMDevice, eConsole, eRender,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::core::Interface;

use super::com::{Apartment, system_error};
use super::devices::{device_id, enumerator, properties};
use crate::audio::backend::{BackendError, OutputKind};
use crate::audio::outputs::output_kind;
use crate::audio::routing::{EndpointAudio, EndpointVolume};

/// Every active render endpoint, Bluetooth hands-free ones included (an app
/// can play there too). An endpoint whose ID can't be read is left out; a
/// name, kind, volume or session that can't be read is left empty, which
/// [`EndpointAudio`] describes.
pub(super) fn endpoint_audio() -> Result<Vec<EndpointAudio>, BackendError> {
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
    let mut endpoints = Vec::new();
    for index in 0..count {
        // SAFETY: an index below the count; a device that fails is skipped.
        let Ok(device) = (unsafe { devices.Item(index) }) else {
            continue;
        };
        let Ok(id) = device_id(&device) else {
            continue;
        };
        // The property store is read by the listing's own reader, which
        // clears each PROPVARIANT once.
        let (name, virtual_device) = match properties(&device) {
            Ok(properties) => {
                let enumerator = properties.enumerator.as_deref().unwrap_or_default();
                let kind = output_kind(properties.form_factor, enumerator, properties.display_port);
                (
                    properties.name.unwrap_or_default(),
                    kind == OutputKind::Virtual,
                )
            }
            Err(_) => (String::new(), false),
        };
        endpoints.push(EndpointAudio {
            is_default: default_id.as_deref() == Some(id.as_str()),
            id,
            name,
            virtual_device,
            volume: volume(&device),
            active_pids: active_session_pids(&device),
        });
    }
    Ok(endpoints)
}

/// The endpoint's mute and master volume, or `None` when they can't be read.
fn volume(device: &IMMDevice) -> Option<EndpointVolume> {
    // SAFETY: activates the endpoint's volume control on a live device and
    // reads two values; nothing is set.
    unsafe {
        let control = device
            .Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None)
            .ok()?;
        let muted = control.GetMute().ok()?.as_bool();
        let level = control.GetMasterVolumeLevelScalar().ok()?;
        Some(EndpointVolume { muted, level })
    }
}

/// The IDs of the processes with an active audio session on the endpoint,
/// sorted. The system sounds session and sessions that aren't active
/// (paused, or expired) are left out, and so is a session that fails.
fn active_session_pids(device: &IMMDevice) -> Vec<u32> {
    let mut pids = Vec::new();
    // SAFETY: calls on live COM objects this function owns; only reads.
    unsafe {
        let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
            return pids;
        };
        let Ok(sessions) = manager.GetSessionEnumerator() else {
            return pids;
        };
        let Ok(total) = sessions.GetCount() else {
            return pids;
        };
        for number in 0..total {
            let Ok(session) = sessions.GetSession(number) else {
                continue;
            };
            if !session
                .GetState()
                .is_ok_and(|state| state == AudioSessionStateActive)
            {
                continue;
            }
            let Ok(session) = session.cast::<IAudioSessionControl2>() else {
                continue;
            };
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
    pids.sort_unstable();
    pids.dedup();
    pids
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads this PC's endpoints, silently and changing nothing. Ignored by
    /// default since it needs Windows audio; run it on a PC with
    /// `cargo test real_endpoints -- --ignored --nocapture`.
    #[test]
    #[ignore = "reads the PC's own audio endpoints"]
    fn real_endpoints_are_read_without_changing_them() {
        let before = endpoint_audio().expect("the endpoints can be read");
        for endpoint in &before {
            println!(
                "{:?}: default {}, virtual {}, volume {:?}, active sessions {:?}",
                endpoint.name,
                endpoint.is_default,
                endpoint.virtual_device,
                endpoint.volume,
                endpoint.active_pids
            );
            assert!(!endpoint.id.is_empty());
            if let Some(volume) = endpoint.volume {
                assert!((0.0..=1.0).contains(&volume.level), "{volume:?}");
            }
        }
        assert!(before.iter().filter(|e| e.is_default).count() <= 1);
        let after = endpoint_audio().expect("the endpoints can be read again");
        let settings = |list: &[EndpointAudio]| -> Vec<(String, Option<EndpointVolume>)> {
            list.iter().map(|e| (e.id.clone(), e.volume)).collect()
        };
        assert_eq!(settings(&before), settings(&after), "nothing changed");
    }
}
