//! Which outputs play, at what delay and gain, and in what format, and what
//! kind of device each is.
//!
//! The user's settings are kept per endpoint ID, whether the device is
//! connected or not. The output policy of the core decides which renderers to
//! add and remove (spec S117, S161); this module turns each output's settings
//! into the delay and gain its renderer applies (S3, S71, S78) and picks the
//! format its stream uses.
//!
//! On macOS, local outputs are delayed to line up with AirPlay speakers, so
//! they play at once while no AirPlay speaker is chosen (S78) and a new output
//! starts at 2000 ms (S77). The Windows app has no AirPlay yet, so there is
//! nothing to line up with: each output plays at the delay the user set (used
//! to line outputs up with each other, such as a Bluetooth speaker with wired
//! ones), and a new output starts at 0 ms.

use std::collections::BTreeMap;

use audiosync_core::policy::delay::{DEFAULT_DELAY_MS, effective_delay_ms, sanitize_delay_ms};
use audiosync_core::policy::session::sync_outputs;
use audiosync_core::reader::delay_ms_to_frames;
use audiosync_core::ring::is_supported_rate;
use audiosync_core::volume::{
    DEFAULT_LOCAL_POSITION, DEFAULT_MASTER_POSITION, output_gain, sanitize_position,
};

use super::SESSION_RATE;
use super::backend::{BackendError, OutputKind};

/// A connected render endpoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
    /// The endpoint ID.
    pub id: String,
    /// The name Windows shows for it.
    pub name: String,
    /// Whether it is Windows' default output.
    pub is_default: bool,
    /// What kind of device it is.
    pub kind: OutputKind,
}

/// `EndpointFormFactor` values, as Windows documents them.
pub mod form_factor {
    /// `Speakers`.
    pub const SPEAKERS: u32 = 1;
    /// `LineLevel`: a line out.
    pub const LINE_LEVEL: u32 = 2;
    /// `Headphones`.
    pub const HEADPHONES: u32 = 3;
    /// `Headset`.
    pub const HEADSET: u32 = 5;
    /// `DigitalAudioDisplayDevice`: a display or TV, over HDMI or
    /// DisplayPort.
    pub const DIGITAL_AUDIO_DISPLAY: u32 = 9;
}

/// The kind of an output, from what Windows says about its endpoint: its form
/// factor (`PKEY_AudioEndpoint_FormFactor`, an `EndpointFormFactor` value),
/// the enumerator of its device (`PKEY_Device_EnumeratorName`: `HDAUDIO` for
/// the PC's own audio, `USB`, `BTHENUM` and other `BTH` names for Bluetooth,
/// `ROOT` for most software devices), and whether its jack is DisplayPort
/// (`PKEY_AudioEndpoint_JackSubType`).
///
/// The enumerator decides first, since a USB or Bluetooth headset reports the
/// same form factor as the PC's headphone jack; any other enumerator leaves it
/// to the form factor.
pub fn output_kind(form_factor: Option<u32>, enumerator: &str, display_port: bool) -> OutputKind {
    let bus = enumerator.to_ascii_uppercase();
    if bus.starts_with("BTH") {
        return OutputKind::Bluetooth;
    }
    match bus.as_str() {
        "USB" => return OutputKind::Usb,
        "ROOT" => return OutputKind::Virtual,
        _ => {}
    }
    match form_factor {
        Some(form_factor::DIGITAL_AUDIO_DISPLAY) if display_port => OutputKind::DisplayPort,
        Some(form_factor::DIGITAL_AUDIO_DISPLAY) => OutputKind::Hdmi,
        Some(form_factor::HEADPHONES | form_factor::HEADSET) => OutputKind::Headphones,
        Some(form_factor::SPEAKERS | form_factor::LINE_LEVEL) => OutputKind::BuiltIn,
        _ => OutputKind::Other,
    }
}

/// Whether AirPlay speakers exist to line local outputs up with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AirPlay {
    /// No AirPlay engine on this platform: outputs play at their own delay.
    Unavailable,
    /// An AirPlay engine runs; `chosen` says whether a speaker is chosen (S79).
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Windows has no AirPlay engine yet; the rule is tested"
        )
    )]
    Available {
        /// Whether an AirPlay speaker is chosen.
        chosen: bool,
    },
}

impl AirPlay {
    /// The delay of an output the user has never set, in ms.
    pub fn default_delay_ms(self) -> f64 {
        match self {
            AirPlay::Unavailable => 0.0,
            AirPlay::Available { .. } => DEFAULT_DELAY_MS,
        }
    }

    /// The delay an output with this configured delay plays at, in ms.
    pub fn effective_delay_ms(self, configured_ms: f64) -> f64 {
        match self {
            AirPlay::Unavailable => configured_ms,
            AirPlay::Available { chosen } => effective_delay_ms(configured_ms, chosen),
        }
    }
}

/// One output's settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputSettings {
    /// Whether the user turned it on.
    pub enabled: bool,
    /// The configured delay in ms, 0 to 20 000.
    pub delay_ms: f64,
    /// The volume slider position, 0 to 100.
    pub volume: f64,
}

/// What a renderer applies.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OutputParams {
    /// The delay in frames at [`SESSION_RATE`].
    pub delay_frames: i64,
    /// The linear gain: the output's volume times the master volume.
    pub gain: f32,
}

/// The renderers to add and remove.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OutputPlan {
    /// Enabled, connected endpoints without a renderer, by ID.
    pub attach: Vec<Endpoint>,
    /// IDs with a renderer that are no longer enabled, sorted.
    pub detach: Vec<String>,
}

impl OutputPlan {
    /// Whether nothing needs to change.
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.attach.is_empty() && self.detach.is_empty()
    }
}

/// Every output's settings, the master volume, and the decisions made from
/// them.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputPolicy {
    settings: BTreeMap<String, OutputSettings>,
    master: f64,
    airplay: AirPlay,
}

impl OutputPolicy {
    /// No output set yet, the master at its default.
    pub fn new(airplay: AirPlay) -> OutputPolicy {
        OutputPolicy {
            settings: BTreeMap::new(),
            master: DEFAULT_MASTER_POSITION,
            airplay,
        }
    }

    /// The settings of output `id`: what the user set, or the defaults (off,
    /// the default delay, volume 60).
    pub fn settings(&self, id: &str) -> OutputSettings {
        self.settings.get(id).copied().unwrap_or(OutputSettings {
            enabled: false,
            delay_ms: self.airplay.default_delay_ms(),
            volume: DEFAULT_LOCAL_POSITION,
        })
    }

    fn entry(&mut self, id: &str) -> &mut OutputSettings {
        let defaults = self.settings(id);
        self.settings.entry(id.to_owned()).or_insert(defaults)
    }

    /// Turns output `id` on or off.
    pub fn set_enabled(&mut self, id: &str, enabled: bool) {
        self.entry(id).enabled = enabled;
    }

    /// Sets the configured delay of output `id`. Returns false when the value
    /// was ignored because it isn't finite; others are clamped to 0 to 20 000.
    pub fn set_delay_ms(&mut self, id: &str, delay_ms: f64) -> bool {
        match sanitize_delay_ms(delay_ms) {
            Some(delay_ms) => {
                self.entry(id).delay_ms = delay_ms;
                true
            }
            None => false,
        }
    }

    /// Sets the volume slider position of output `id`. Returns false when the
    /// value was ignored because it isn't finite; others are clamped.
    pub fn set_volume(&mut self, id: &str, position: f64) -> bool {
        match sanitize_position(position) {
            Some(position) => {
                self.entry(id).volume = position;
                true
            }
            None => false,
        }
    }

    /// Sets the master slider position. Returns false when the value was
    /// ignored because it isn't finite; others are clamped.
    pub fn set_master(&mut self, position: f64) -> bool {
        match sanitize_position(position) {
            Some(position) => {
                self.master = position;
                true
            }
            None => false,
        }
    }

    /// The master slider position.
    pub fn master(&self) -> f64 {
        self.master
    }

    /// Changes the AirPlay state. Returns true when the delay of outputs
    /// changes because of it, so running renderers need new delays (S80).
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "Windows has no AirPlay engine yet; the rule is tested"
        )
    )]
    pub fn set_airplay(&mut self, airplay: AirPlay) -> bool {
        let changed = airplay != self.airplay;
        self.airplay = airplay;
        changed
    }

    /// The IDs of the enabled outputs, sorted.
    pub fn enabled_ids(&self) -> Vec<&str> {
        self.settings
            .iter()
            .filter(|(_, settings)| settings.enabled)
            .map(|(id, _)| id.as_str())
            .collect()
    }

    /// The delay and gain a renderer for output `id` applies: its effective
    /// delay in frames at the session rate (S3, S78) and the product of its
    /// volume and the master volume on the cubic curve (S70, S71).
    pub fn params(&self, id: &str) -> OutputParams {
        let settings = self.settings(id);
        let delay_ms = self.airplay.effective_delay_ms(settings.delay_ms);
        OutputParams {
            delay_frames: delay_ms_to_frames(delay_ms, SESSION_RATE).unwrap_or(0),
            gain: output_gain(settings.volume, self.master) as f32,
        }
    }

    /// The renderers to add and remove, given the connected endpoints
    /// `present` and the endpoint IDs that have a renderer (`active`), with
    /// the core's output sync (S117, S161): while `running`, enabled and
    /// connected outputs without a renderer are added; outputs that are no
    /// longer enabled are removed. An enabled output that isn't connected
    /// waits until it is.
    pub fn plan(&self, running: bool, present: &[Endpoint], active: &[&str]) -> OutputPlan {
        let present_ids: Vec<&str> = present
            .iter()
            .map(|endpoint| endpoint.id.as_str())
            .collect();
        let sync = sync_outputs(running, &self.enabled_ids(), &present_ids, active);
        let attach = sync
            .add
            .iter()
            .filter_map(|id| present.iter().find(|endpoint| endpoint.id == *id).cloned())
            .collect();
        OutputPlan {
            attach,
            detach: sync.remove,
        }
    }
}

/// An endpoint's mix format, the format the Windows audio engine mixes in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MixFormat {
    /// Whether its samples are 32-bit float.
    pub float32: bool,
    /// Frames per second.
    pub rate: u32,
    /// Channels per frame.
    pub channels: u16,
    /// Which speaker each channel feeds (`dwChannelMask`), 0 when not given.
    pub channel_mask: u32,
}

/// The format an output stream renders in: always 32-bit float, at the
/// endpoint's own rate and channel layout, so Windows doesn't resample; the
/// render pipeline converts from the session rate (S62).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderFormat {
    /// Frames per second.
    pub rate: u32,
    /// Channels per frame.
    pub channels: u16,
    /// Which speaker each channel feeds.
    pub channel_mask: u32,
    /// Whether the mix format isn't float, so the stream asks Windows to
    /// convert the float samples (`AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM`);
    /// otherwise the mix format is used as it is.
    pub convert: bool,
}

/// How an output's stream was set up, for the debug log: its format and the
/// size of its buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamSetup {
    /// The format it renders in.
    pub format: RenderFormat,
    /// The stream buffer, in device frames.
    pub buffer_frames: u32,
}

/// `SPEAKER_FRONT_LEFT | SPEAKER_FRONT_RIGHT`.
const STEREO_MASK: u32 = 0x3;

/// `SPEAKER_FRONT_CENTER`.
const MONO_MASK: u32 = 0x4;

/// The usual channel mask for `channels` channels: front center for mono,
/// front left and right for stereo, none for other layouts (Windows then
/// assigns speakers in its standard order).
pub fn default_channel_mask(channels: u16) -> u32 {
    match channels {
        1 => MONO_MASK,
        2 => STEREO_MASK,
        _ => 0,
    }
}

/// The render format for an endpoint whose mix format is `mix`.
pub fn render_format(mix: MixFormat) -> Result<RenderFormat, BackendError> {
    if mix.channels == 0 || !is_supported_rate(mix.rate) {
        return Err(BackendError::UnsupportedFormat);
    }
    let channel_mask = if mix.channel_mask == 0 {
        default_channel_mask(mix.channels)
    } else {
        mix.channel_mask
    };
    Ok(RenderFormat {
        rate: mix.rate,
        channels: mix.channels,
        channel_mask,
        convert: !mix.float32,
    })
}

/// 100-nanosecond units per millisecond, the unit of WASAPI durations.
pub const HNS_PER_MS: i64 = 10_000;

/// The smallest render buffer, 20 ms.
pub const MIN_RENDER_BUFFER_HNS: i64 = 20 * HNS_PER_MS;

/// The largest render buffer, 200 ms.
pub const MAX_RENDER_BUFFER_HNS: i64 = 200 * HNS_PER_MS;

/// The buffer an event-driven shared-mode render stream asks for, given the
/// endpoint's default period: two periods, so a render thread that wakes up
/// late still finds the device a period ahead, and at least 20 ms (at most
/// 200 ms).
pub fn render_buffer_hns(default_period_hns: i64) -> i64 {
    default_period_hns
        .saturating_mul(2)
        .clamp(MIN_RENDER_BUFFER_HNS, MAX_RENDER_BUFFER_HNS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn endpoint(id: &str) -> Endpoint {
        Endpoint {
            id: id.to_owned(),
            name: format!("Output {id}"),
            is_default: false,
            kind: OutputKind::Other,
        }
    }

    #[test]
    fn the_bus_tells_bluetooth_usb_and_software_devices() {
        let headphones = Some(form_factor::HEADPHONES);
        assert_eq!(
            output_kind(headphones, "BTHENUM", false),
            OutputKind::Bluetooth
        );
        assert_eq!(
            output_kind(headphones, "BthHFEnum", false),
            OutputKind::Bluetooth
        );
        assert_eq!(
            output_kind(Some(form_factor::SPEAKERS), "BTHLEDevice", false),
            OutputKind::Bluetooth
        );
        assert_eq!(
            output_kind(Some(form_factor::HEADSET), "USB", false),
            OutputKind::Usb
        );
        assert_eq!(
            output_kind(Some(form_factor::SPEAKERS), "ROOT", false),
            OutputKind::Virtual
        );
    }

    #[test]
    fn the_form_factor_tells_the_rest() {
        let kind = |form, bus| output_kind(Some(form), bus, false);
        assert_eq!(kind(form_factor::SPEAKERS, "HDAUDIO"), OutputKind::BuiltIn);
        assert_eq!(
            kind(form_factor::LINE_LEVEL, "HDAUDIO"),
            OutputKind::BuiltIn
        );
        assert_eq!(
            kind(form_factor::HEADPHONES, "HDAUDIO"),
            OutputKind::Headphones
        );
        assert_eq!(
            kind(form_factor::HEADSET, "INTELAUDIO"),
            OutputKind::Headphones
        );
        assert_eq!(
            kind(form_factor::DIGITAL_AUDIO_DISPLAY, "HDAUDIO"),
            OutputKind::Hdmi
        );
        assert_eq!(
            output_kind(Some(form_factor::DIGITAL_AUDIO_DISPLAY), "HDAUDIO", true),
            OutputKind::DisplayPort
        );
        assert_eq!(kind(8, "HDAUDIO"), OutputKind::Other, "S/PDIF");
        assert_eq!(kind(0, "SWD"), OutputKind::Other, "a remote session");
        assert_eq!(output_kind(None, "", false), OutputKind::Other);
    }

    fn ids(endpoints: &[Endpoint]) -> Vec<&str> {
        endpoints
            .iter()
            .map(|endpoint| endpoint.id.as_str())
            .collect()
    }

    fn policy_with(enabled: &[&str]) -> OutputPolicy {
        let mut policy = OutputPolicy::new(AirPlay::Unavailable);
        for id in enabled {
            policy.set_enabled(id, true);
        }
        policy
    }

    #[test]
    fn enabled_connected_outputs_play_sorted_by_id() {
        let policy = policy_with(&["c", "a", "b"]);
        let present = [endpoint("b"), endpoint("d"), endpoint("a")];
        let plan = policy.plan(true, &present, &[]);
        assert_eq!(ids(&plan.attach), ["a", "b"]);
        assert!(plan.detach.is_empty());
    }

    #[test]
    fn an_output_that_already_plays_is_not_added_again() {
        let policy = policy_with(&["a", "b"]);
        let plan = policy.plan(true, &[endpoint("a"), endpoint("b")], &["a"]);
        assert_eq!(ids(&plan.attach), ["b"]);
    }

    #[test]
    fn nothing_plays_while_stopped() {
        let policy = policy_with(&["a"]);
        let plan = policy.plan(false, &[endpoint("a")], &[]);
        assert!(plan.is_empty());
    }

    #[test]
    fn an_output_turned_off_is_removed() {
        let mut policy = policy_with(&["a", "b"]);
        policy.set_enabled("b", false);
        let plan = policy.plan(true, &[endpoint("a"), endpoint("b")], &["a", "b"]);
        assert!(plan.attach.is_empty());
        assert_eq!(plan.detach, ["b"]);
    }

    #[test]
    fn an_enabled_output_that_is_not_connected_waits() {
        let policy = policy_with(&["gone"]);
        assert!(policy.plan(true, &[endpoint("other")], &[]).is_empty());
    }

    #[test]
    fn a_renderer_whose_device_disappeared_is_kept_until_the_output_is_turned_off() {
        // The renderer stops by itself when its device goes away (S68); the
        // plan only removes renderers of outputs that are turned off.
        let policy = policy_with(&["a"]);
        assert!(policy.plan(true, &[], &["a"]).is_empty());
    }

    #[test]
    fn a_device_removed_and_added_again_plays_again() {
        let policy = policy_with(&["a"]);
        let present = [endpoint("a")];
        assert_eq!(ids(&policy.plan(true, &present, &[]).attach), ["a"]);
        // Unplugged: its renderer stopped and the device isn't listed.
        assert!(policy.plan(true, &[], &[]).is_empty());
        // Plugged in again: added again, still enabled.
        assert_eq!(ids(&policy.plan(true, &present, &[]).attach), ["a"]);
        assert!(policy.settings("a").enabled);
    }

    #[test]
    fn a_new_output_starts_off_at_volume_60_and_no_delay() {
        let policy = OutputPolicy::new(AirPlay::Unavailable);
        assert_eq!(
            policy.settings("new"),
            OutputSettings {
                enabled: false,
                delay_ms: 0.0,
                volume: 60.0,
            }
        );
    }

    #[test]
    fn with_airplay_a_new_output_starts_at_2000_ms() {
        let policy = OutputPolicy::new(AirPlay::Available { chosen: true });
        assert_eq!(policy.settings("new").delay_ms, 2000.0);
    }

    #[test]
    fn a_delay_becomes_frames_at_the_session_rate() {
        let mut policy = policy_with(&["a"]);
        policy.set_delay_ms("a", 2000.0);
        assert_eq!(policy.params("a").delay_frames, 96_000);
        policy.set_delay_ms("a", 1820.0);
        assert_eq!(policy.params("a").delay_frames, 87_360);
        policy.set_delay_ms("a", 0.0);
        assert_eq!(policy.params("a").delay_frames, 0);
    }

    #[test]
    fn delays_are_clamped_and_non_finite_ones_ignored() {
        let mut policy = policy_with(&["a"]);
        assert!(policy.set_delay_ms("a", 500.0));
        assert!(!policy.set_delay_ms("a", f64::NAN));
        assert!(!policy.set_delay_ms("a", f64::INFINITY));
        assert_eq!(policy.settings("a").delay_ms, 500.0);
        assert!(policy.set_delay_ms("a", 25_000.0));
        assert_eq!(policy.settings("a").delay_ms, 20_000.0);
        assert_eq!(policy.params("a").delay_frames, 960_000);
        assert!(policy.set_delay_ms("a", -5.0));
        assert_eq!(policy.settings("a").delay_ms, 0.0);
    }

    #[test]
    fn each_output_has_its_own_delay_and_volume() {
        let mut policy = policy_with(&["a", "b"]);
        policy.set_delay_ms("a", 150.0);
        policy.set_delay_ms("b", 40.0);
        policy.set_volume("a", 50.0);
        policy.set_volume("b", 100.0);
        assert_eq!(
            policy.params("a"),
            OutputParams {
                delay_frames: 7_200,
                gain: 0.125,
            }
        );
        assert_eq!(
            policy.params("b"),
            OutputParams {
                delay_frames: 1_920,
                gain: 1.0,
            }
        );
    }

    #[test]
    fn the_gain_follows_the_cubic_curve_times_the_master() {
        let mut policy = policy_with(&["a"]);
        assert!((policy.params("a").gain - 0.216).abs() < 1e-6, "volume 60");
        policy.set_volume("a", 50.0);
        policy.set_master(50.0);
        assert_eq!(policy.params("a").gain, 0.015_625);
        policy.set_master(0.0);
        assert_eq!(policy.params("a").gain, 0.0);
    }

    #[test]
    fn volumes_are_clamped_and_non_finite_ones_ignored() {
        let mut policy = policy_with(&["a"]);
        assert!(!policy.set_volume("a", f64::NAN));
        assert_eq!(policy.settings("a").volume, 60.0);
        assert!(policy.set_volume("a", 150.0));
        assert_eq!(policy.settings("a").volume, 100.0);
        assert!(!policy.set_master(f64::NEG_INFINITY));
        assert_eq!(policy.master(), 100.0);
        assert!(policy.set_master(-3.0));
        assert_eq!(policy.master(), 0.0);
    }

    #[test]
    fn settings_of_a_disconnected_output_are_kept() {
        let mut policy = OutputPolicy::new(AirPlay::Unavailable);
        policy.set_volume("headphones", 30.0);
        policy.set_delay_ms("headphones", 250.0);
        let settings = policy.settings("headphones");
        assert_eq!(settings.volume, 30.0);
        assert_eq!(settings.delay_ms, 250.0);
        assert!(!settings.enabled);
    }

    #[test]
    fn with_airplay_and_no_speaker_chosen_outputs_play_at_once() {
        let mut policy = policy_with(&["a"]);
        policy.set_delay_ms("a", 2000.0);
        assert!(policy.set_airplay(AirPlay::Available { chosen: false }));
        assert_eq!(policy.params("a").delay_frames, 0);
        assert!(policy.set_airplay(AirPlay::Available { chosen: true }));
        assert_eq!(policy.params("a").delay_frames, 96_000);
        assert!(!policy.set_airplay(AirPlay::Available { chosen: true }));
        assert_eq!(policy.settings("a").delay_ms, 2000.0, "the setting is kept");
    }

    fn mix(float32: bool, rate: u32, channels: u16, channel_mask: u32) -> MixFormat {
        MixFormat {
            float32,
            rate,
            channels,
            channel_mask,
        }
    }

    #[test]
    fn a_float_mix_format_is_used_as_it_is() {
        assert_eq!(
            render_format(mix(true, 48_000, 2, 0x3)),
            Ok(RenderFormat {
                rate: 48_000,
                channels: 2,
                channel_mask: 0x3,
                convert: false,
            })
        );
    }

    #[test]
    fn another_mix_format_gets_float_converted_by_windows_at_its_own_rate() {
        assert_eq!(
            render_format(mix(false, 44_100, 6, 0x3f)),
            Ok(RenderFormat {
                rate: 44_100,
                channels: 6,
                channel_mask: 0x3f,
                convert: true,
            })
        );
    }

    #[test]
    fn a_missing_channel_mask_gets_the_usual_one() {
        assert_eq!(
            render_format(mix(true, 48_000, 1, 0)).unwrap().channel_mask,
            0x4
        );
        assert_eq!(
            render_format(mix(true, 48_000, 2, 0)).unwrap().channel_mask,
            0x3
        );
        assert_eq!(
            render_format(mix(true, 48_000, 8, 0)).unwrap().channel_mask,
            0
        );
    }

    #[test]
    fn unusable_mix_formats_are_refused() {
        assert_eq!(
            render_format(mix(true, 48_000, 0, 0)),
            Err(BackendError::UnsupportedFormat)
        );
        assert_eq!(
            render_format(mix(true, 768_000, 2, 0x3)),
            Err(BackendError::UnsupportedFormat)
        );
    }

    #[test]
    fn the_render_buffer_holds_two_periods_within_bounds() {
        assert_eq!(render_buffer_hns(100_000), 200_000, "10 ms period");
        assert_eq!(
            render_buffer_hns(30_000),
            MIN_RENDER_BUFFER_HNS,
            "3 ms period"
        );
        assert_eq!(render_buffer_hns(300_000), 600_000, "30 ms period");
        assert_eq!(render_buffer_hns(50_000_000), MAX_RENDER_BUFFER_HNS);
        assert_eq!(render_buffer_hns(0), MIN_RENDER_BUFFER_HNS);
        assert_eq!(render_buffer_hns(-1), MIN_RENDER_BUFFER_HNS);
        assert_eq!(render_buffer_hns(i64::MAX), MAX_RENDER_BUFFER_HNS);
    }
}
