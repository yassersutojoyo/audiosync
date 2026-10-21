//! What Windows reports about each active render endpoint's latency, for
//! comparing with the delays users set by hand on Bluetooth outputs. Ignored
//! by default since it needs Windows audio; run it on a PC with
//! `cargo test latency_probe -- --ignored --nocapture`.
//!
//! Each stream plays only silence (`AUDCLNT_BUFFERFLAGS_SILENT`) for
//! [`SILENT_RUN`] and stops, so the clock can say how far behind the written
//! audio the device plays; nothing is heard and no setting changes.

use std::mem::size_of;
use std::thread;
use std::time::{Duration, Instant};

use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_NOPERSIST, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, DEVICE_STATE,
    DEVICE_STATE_ACTIVE, DEVICE_STATEMASK_ALL, IAudioClient, IAudioClient3, IAudioClock,
    IAudioRenderClient, IDeviceTopology, IMMDevice, IMMDeviceEnumerator, WAVEFORMATEX, eRender,
};
use windows::Win32::Media::KernelStreaming::{
    IKsControl, KSIDENTIFIER, KSIDENTIFIER_0, KSIDENTIFIER_0_0, KSNODEPROPERTY,
    KSPROPERTY_AUDIO_LATENCY, KSPROPERTY_TYPE_GET, KSPROPERTY_TYPE_TOPOLOGY, KSPROPSETID_Audio,
    KSTIME,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::core::HSTRING;

use super::com::{Apartment, ComMemory, float_format, read_mix_format, take_com_string};
use super::devices::{device_id, enumerator, properties};
use crate::audio::outputs::{render_buffer_hns, render_format};

const SILENT_RUN: Duration = Duration::from_millis(600);
const HNS_PER_MS: f64 = 10_000.0;
/// KS topology nodes tried for `KSPROPERTY_AUDIO_LATENCY`.
const KS_NODES: u32 = 32;

#[test]
#[ignore = "plays silence on the PC's own audio endpoints"]
fn latency_probe() {
    let _apartment = Apartment::enter().expect("COM");
    let enumerator = enumerator().expect("the enumerator");
    // SAFETY: calls on the live enumerator and collection, indices below
    // the count.
    let devices = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }
        .expect("the render endpoints");
    let count = unsafe { devices.GetCount() }.expect("their count");
    for index in 0..count {
        let device = unsafe { devices.Item(index) }.expect("an endpoint");
        let props = properties(&device).unwrap_or_default();
        println!(
            "== {:?}: enumerator {:?}, form factor {:?}, id {}",
            props.name.as_deref().unwrap_or("?"),
            props.enumerator.as_deref().unwrap_or("?"),
            props.form_factor,
            device_id(&device).unwrap_or_default()
        );
        match probe(&device) {
            Ok(lines) => lines.iter().for_each(|line| println!("   {line}")),
            Err(error) => println!("   failed: {error}"),
        }
        println!("   ks: {}", ks_latency(&enumerator, &device));
    }
    // Paired Bluetooth outputs that aren't connected can't be measured; they
    // are named so a run says which ones to connect. SAFETY: as above.
    let all = unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE(DEVICE_STATEMASK_ALL)) }
        .expect("every render endpoint");
    for index in 0..unsafe { all.GetCount() }.expect("their count") {
        let device = unsafe { all.Item(index) }.expect("an endpoint");
        let state = unsafe { device.GetState() }.unwrap_or_default();
        let props = properties(&device).unwrap_or_default();
        if state != DEVICE_STATE_ACTIVE
            && props
                .enumerator
                .as_deref()
                .is_some_and(|bus| bus.starts_with("BTH"))
        {
            println!(
                "-- not active (state {}): {:?}, enumerator {:?}",
                state.0,
                props.name.as_deref().unwrap_or("?"),
                props.enumerator.as_deref().unwrap_or("?")
            );
        }
    }
}

fn probe(device: &IMMDevice) -> windows::core::Result<Vec<String>> {
    let mut lines = Vec::new();
    // SAFETY: calls on COM objects this thread owns; the mix format is freed
    // by ComMemory after its last use, and the formats passed to Initialize
    // and GetSharedModeEnginePeriod live through the calls.
    unsafe {
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let mix = ComMemory::new(client.GetMixFormat()?);
        let mix_format = read_mix_format(mix.as_ptr());
        lines.push(format!("mix format: {mix_format:?}"));
        let (mut default_period, mut min_period) = (0i64, 0i64);
        client.GetDevicePeriod(Some(&raw mut default_period), Some(&raw mut min_period))?;
        lines.push(format!(
            "device period: default {:.2} ms, minimum {:.2} ms",
            default_period as f64 / HNS_PER_MS,
            min_period as f64 / HNS_PER_MS
        ));
        if let Ok(client3) = device.Activate::<IAudioClient3>(CLSCTX_ALL, None) {
            let (mut default, mut fundamental, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
            let engine = client3.GetSharedModeEnginePeriod(
                mix.as_ptr().cast_const(),
                &raw mut default,
                &raw mut fundamental,
                &raw mut min,
                &raw mut max,
            );
            lines.push(match engine {
                Ok(()) => format!(
                    "engine period (frames): default {default}, fundamental {fundamental}, \
                     min {min}, max {max}"
                ),
                Err(error) => format!("engine period: {error}"),
            });
        }
        let Ok(format) = render_format(mix_format) else {
            lines.push("render format: unsupported".to_owned());
            return Ok(lines);
        };
        let own = float_format(format);
        let (wave, flags) = if format.convert {
            (
                (&raw const own).cast::<WAVEFORMATEX>(),
                AUDCLNT_STREAMFLAGS_NOPERSIST
                    | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                    | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            )
        } else {
            (mix.as_ptr().cast_const(), AUDCLNT_STREAMFLAGS_NOPERSIST)
        };
        let buffer_hns = render_buffer_hns(default_period);
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, buffer_hns, 0, wave, None)?;
        let buffer = client.GetBufferSize()?;
        let latency = client.GetStreamLatency()?;
        lines.push(format!(
            "stream (as AudioSync opens it, {:.0} ms asked): buffer {buffer} frames = {:.2} ms, \
             GetStreamLatency {:.2} ms",
            buffer_hns as f64 / HNS_PER_MS,
            f64::from(buffer) * 1000.0 / f64::from(format.rate),
            latency as f64 / HNS_PER_MS
        ));
        lines.push(silent_run(&client, buffer, format.rate)?);
    }
    Ok(lines)
}

/// Plays silence for [`SILENT_RUN`], then reports how many written frames
/// the clock says haven't been played yet: the queued buffer (padding) plus
/// whatever Windows knows lies beyond it.
///
/// # Safety
///
/// `client` is initialized for shared-mode rendering with a buffer of
/// `buffer` frames, on this thread.
unsafe fn silent_run(
    client: &IAudioClient,
    buffer: u32,
    rate: u32,
) -> windows::core::Result<String> {
    // SAFETY: as the caller guarantees; every buffer taken is released
    // marked silent, so its contents are never read.
    unsafe {
        let render: IAudioRenderClient = client.GetService()?;
        let clock: IAudioClock = client.GetService()?;
        let frequency = clock.GetFrequency()?;
        let mut written = 0u64;
        let fill = |written: &mut u64| -> windows::core::Result<()> {
            let free = buffer - client.GetCurrentPadding()?;
            if free > 0 {
                render.GetBuffer(free)?;
                render.ReleaseBuffer(free, AUDCLNT_BUFFERFLAGS_SILENT.0 as u32)?;
                *written += u64::from(free);
            }
            Ok(())
        };
        fill(&mut written)?;
        client.Start()?;
        let started = Instant::now();
        let mut samples = Vec::new();
        while started.elapsed() < SILENT_RUN {
            thread::sleep(Duration::from_millis(3));
            let padding = client.GetCurrentPadding()?;
            fill(&mut written)?;
            if started.elapsed() > SILENT_RUN / 2 {
                let mut position = 0u64;
                clock.GetPosition(&raw mut position, None)?;
                let played = position as f64 * f64::from(rate) / frequency as f64;
                samples.push((written as f64 - played, f64::from(padding)));
            }
        }
        client.Stop()?;
        let ms = |frames: f64| frames * 1000.0 / f64::from(rate);
        let depth: Vec<f64> = samples.iter().map(|(depth, _)| ms(*depth)).collect();
        let padding: Vec<f64> = samples.iter().map(|(_, padding)| ms(*padding)).collect();
        let range = |values: &[f64]| {
            let min = values.iter().copied().fold(f64::INFINITY, f64::min);
            let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
            let mean = values.iter().sum::<f64>() / values.len().max(1) as f64;
            format!("{min:.1}..{max:.1} (mean {mean:.1})")
        };
        Ok(format!(
            "clock after {} ms of silence ({} samples): written minus played {} ms, \
             padding before each refill {} ms",
            SILENT_RUN.as_millis(),
            samples.len(),
            range(&depth),
            range(&padding)
        ))
    }
}

/// `KSPROPERTY_AUDIO_LATENCY` of the first topology nodes of the KS filter
/// behind `device`, or why it couldn't be read.
fn ks_latency(enumerator: &IMMDeviceEnumerator, device: &IMMDevice) -> String {
    // SAFETY: calls on COM objects this thread owns; the property request
    // and its KSTIME answer live on this stack through each call, and the
    // connected device ID is copied and freed by take_com_string.
    unsafe {
        let filter = (|| -> windows::core::Result<IKsControl> {
            let topology: IDeviceTopology = device.Activate(CLSCTX_ALL, None)?;
            let connector = topology.GetConnector(0)?;
            let id = take_com_string(connector.GetDeviceIdConnectedTo()?).unwrap_or_default();
            enumerator
                .GetDevice(&HSTRING::from(id))?
                .Activate(CLSCTX_ALL, None)
        })();
        let filter = match filter {
            Ok(filter) => filter,
            Err(error) => return format!("no KS filter ({error})"),
        };
        let mut found = Vec::new();
        let mut last_error = None;
        for node in 0..KS_NODES {
            let request = KSNODEPROPERTY {
                Property: KSIDENTIFIER {
                    Anonymous: KSIDENTIFIER_0 {
                        Anonymous: KSIDENTIFIER_0_0 {
                            Set: KSPROPSETID_Audio,
                            Id: KSPROPERTY_AUDIO_LATENCY.0 as u32,
                            Flags: KSPROPERTY_TYPE_GET | KSPROPERTY_TYPE_TOPOLOGY,
                        },
                    },
                },
                NodeId: node,
                Reserved: 0,
            };
            let mut time = KSTIME::default();
            let mut returned = 0u32;
            match filter.KsProperty(
                (&raw const request).cast::<KSIDENTIFIER>(),
                size_of::<KSNODEPROPERTY>() as u32,
                (&raw mut time).cast(),
                size_of::<KSTIME>() as u32,
                &raw mut returned,
            ) {
                Ok(()) => {
                    let ms = if time.Denominator == 0 {
                        f64::NAN
                    } else {
                        time.Time as f64 * f64::from(time.Numerator)
                            / f64::from(time.Denominator)
                            / HNS_PER_MS
                    };
                    found.push(format!("node {node}: {ms:.2} ms"));
                }
                Err(error) => last_error = Some(error),
            }
        }
        if found.is_empty() {
            format!(
                "AUDIO_LATENCY on nodes 0..{KS_NODES}: none ({})",
                last_error
                    .map(|error| error.to_string())
                    .unwrap_or_default()
            )
        } else {
            found.join(", ")
        }
    }
}
