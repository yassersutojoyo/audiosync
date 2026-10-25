//! The C interface: a plain C ABI, declared by the hand-written
//! header `include/audiosync_core.h`.
//!
//! Audio crosses the boundary a whole buffer at a time: a pointer to
//! interleaved stereo float32 frames plus a frame count, never per sample. The
//! real-time calls never allocate, lock, wait, make a system call or panic
//! (S126): bad arguments make them do nothing or saturate.
//!
//! - This module: the version, the volume curve and the equalizer, plus the
//!   helpers that turn the host's pointers into slices.
//! - [`ring`]: the delay ring, its single writer and its readers (S35-S59).
//! - [`render`]: the resampler, a local output's output stage (S62-S65) and
//!   its whole renderer with the fades (S182-S188).
//! - [`sync`]: the AirPlay sync rule and the delay helpers (S3, S77-S80).
//! - [`device_volume`]: what an output device's own volume and mute mean for
//!   its row, and what the one-click fix writes (S192-S202).
//! - [`calibrate`]: the microphone calibration's test signal and estimate
//!   (S232-S234).
//! - [`calibrate_all`]: Calibrate All Speakers, a run the host drives step
//!   by step and its results sheet.
//! - [`zones`]: a zone click's plan and the migration of groups to zones
//!   (S246-S248).
//! - [`presets`]: what a speaker preset keeps, a recall's plan and the
//!   current-preset check (S251-S253).
//! - [`airplay`]: AudioSync's own AirPlay sender: discovery, speakers, their
//!   events and the capture's input.
//! - [`cast`]: AudioSync's own Google Cast sender: discovery, one device's
//!   session, its events, the device memory and the capture's input.
//! - [`remote`]: AudioSync's web remote: its server, the snapshots the host
//!   publishes, the pairing code and the host's command callback. These
//!   three are the parts of this interface that run threads and sockets of
//!   their own.
//!
//! Threading (S113, S132): each handle states which thread makes which call.
//! A handle that an audio thread shares with other threads keeps the two
//! sides in separate fields that share only atomics, and each call borrows
//! only its own side, never the whole handle. For an equalizer,
//! `asc_eq_process` runs on one capture thread at a time and borrows the
//! [`EqProcessor`]; every other `asc_eq_*` call runs on one control thread at
//! a time and borrows the [`EqControl`]; `asc_eq_destroy` runs once no other
//! call on the handle can run.
//!
//! This module and its submodules hold all of the crate's `unsafe` code; each
//! block states why it is sound.

#![warn(clippy::undocumented_unsafe_blocks)]

use std::ffi::{CStr, c_char};

use crate::eq::{
    self, BAND_CENTERS_HZ, BAND_COUNT, EqControl, EqProcessor, EqSettings, PRESET_COUNT, PRESETS,
};
use crate::volume;

pub mod airplay;
pub mod calibrate;
pub mod calibrate_all;
pub mod cast;
pub mod device_volume;
pub mod presets;
pub mod remote;
pub mod render;
pub mod ring;
pub mod source_quit;
pub mod sync;
pub mod zones;

pub use self::airplay::*;
pub use self::calibrate::*;
pub use self::calibrate_all::*;
pub use self::cast::*;
pub use self::device_volume::*;
pub use self::presets::*;
pub use self::remote::*;
pub use self::render::*;
pub use self::ring::*;
pub use self::source_quit::*;
pub use self::sync::*;
pub use self::zones::*;

/// Version of this C interface, bumped on any incompatible change; the header's
/// `ASC_ABI_VERSION`. Adding functions is compatible.
pub const ABI_VERSION: u32 = 1;

const VERSION_C: &CStr =
    match CStr::from_bytes_with_nul(concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes()) {
        Ok(version) => version,
        Err(_) => panic!("the crate version contains a NUL byte"),
    };

/// The preset identifiers as C strings, in menu order (checked against
/// [`PRESETS`] by a test).
const PRESET_IDS: [&CStr; PRESET_COUNT] = [
    c"flat",
    c"bassBoost",
    c"bassCut",
    c"trebleBoost",
    c"vocal",
    c"rock",
    c"pop",
    c"jazz",
    c"classical",
    c"electronic",
    c"loudness",
    c"smallSpeakers",
];

/// An equalizer instance behind an opaque C pointer.
pub struct AscEqualizer {
    control: EqControl,
    processor: EqProcessor,
}

/// The number of samples in `frames` frames of `channels` channels, if such
/// a float buffer can exist.
fn samples_for(frames: usize, channels: usize) -> Option<usize> {
    frames
        .checked_mul(channels)
        .filter(|&samples| samples <= isize::MAX as usize / size_of::<f32>())
}

/// The number of samples in `frames` stereo frames, if such a buffer can exist.
fn stereo_samples(frames: usize) -> Option<usize> {
    samples_for(frames, 2)
}

/// `len` of the host's elements at `ptr` as a slice: empty when `len` is 0
/// (`ptr` may then be null); `None` for a null `ptr` with a nonzero `len`, or
/// for a length no buffer can have.
///
/// # Safety
///
/// Unless null, `ptr` points to `len` aligned, initialized elements that
/// nothing writes during `'a`.
unsafe fn host_slice<'a, T>(ptr: *const T, len: usize) -> Option<&'a [T]> {
    if len == 0 {
        return Some(&[]);
    }
    if ptr.is_null() || len > isize::MAX as usize / size_of::<T>().max(1) {
        return None;
    }
    // SAFETY: non-null; by the caller's contract `len` aligned, initialized
    // elements that nothing writes during 'a; the size fits in isize.
    Some(unsafe { std::slice::from_raw_parts(ptr, len) })
}

/// `len` of the host's writable elements at `ptr` as a slice, by the rules
/// of [`host_slice`].
///
/// # Safety
///
/// Unless null, `ptr` points to `len` aligned, initialized elements that
/// nothing else reads or writes during `'a`.
unsafe fn host_slice_mut<'a, T>(ptr: *mut T, len: usize) -> Option<&'a mut [T]> {
    if len == 0 {
        return Some(&mut []);
    }
    if ptr.is_null() || len > isize::MAX as usize / size_of::<T>().max(1) {
        return None;
    }
    // SAFETY: non-null; by the caller's contract `len` aligned, initialized
    // elements that only this borrow touches during 'a; the size fits in
    // isize.
    Some(unsafe { std::slice::from_raw_parts_mut(ptr, len) })
}

/// The crate version as a static C string.
#[unsafe(no_mangle)]
pub extern "C" fn asc_version() -> *const c_char {
    VERSION_C.as_ptr()
}

/// The C interface version ([`ABI_VERSION`]).
#[unsafe(no_mangle)]
pub extern "C" fn asc_abi_version() -> u32 {
    ABI_VERSION
}

/// [`volume::gain_for_position`] (S70).
#[unsafe(no_mangle)]
pub extern "C" fn asc_volume_gain(position: f64) -> f64 {
    volume::gain_for_position(position)
}

/// [`volume::position_for_gain`] (S70).
#[unsafe(no_mangle)]
pub extern "C" fn asc_volume_position(gain: f64) -> f64 {
    volume::position_for_gain(gain)
}

/// [`volume::output_gain`]: a local output's gain from its own position and
/// the master position (S71).
#[unsafe(no_mangle)]
pub extern "C" fn asc_output_gain(local_position: f64, master_position: f64) -> f64 {
    volume::output_gain(local_position, master_position)
}

/// [`volume::zoned_output_gain`]: a local output's gain through its zone,
/// 100 outside a zone (S71, S249).
#[unsafe(no_mangle)]
pub extern "C" fn asc_zone_output_gain(
    local_position: f64,
    zone_position: f64,
    master_position: f64,
) -> f64 {
    volume::zoned_output_gain(local_position, zone_position, master_position)
}

/// [`volume::apply_gain`] on `frames` interleaved stereo frames, in place (S64, S65).
/// Real-time safe.
///
/// # Safety
///
/// `interleaved` is null or points to `frames` × 2 aligned floats, readable
/// and writable, that nothing else accesses during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_apply_gain(interleaved: *mut f32, frames: usize, gain: f32) {
    let Some(samples) = stereo_samples(frames) else {
        return;
    };
    if interleaved.is_null() || samples == 0 {
        return;
    }
    // SAFETY: the pointer is non-null and, by the caller's contract, addresses
    // `samples` aligned floats that only this call accesses.
    let block = unsafe { std::slice::from_raw_parts_mut(interleaved, samples) };
    volume::apply_gain(block, gain);
}

/// The number of equalizer bands (S22).
#[unsafe(no_mangle)]
pub extern "C" fn asc_eq_band_count() -> usize {
    BAND_COUNT
}

/// A band's center in Hz, or 0 for a bad index (S22).
#[unsafe(no_mangle)]
pub extern "C" fn asc_eq_band_center_hz(band: usize) -> f64 {
    BAND_CENTERS_HZ.get(band).copied().unwrap_or(0.0)
}

/// The number of presets (S33).
#[unsafe(no_mangle)]
pub extern "C" fn asc_eq_preset_count() -> usize {
    PRESET_COUNT
}

/// A preset's identifier as a static C string, or null for a bad index (S33).
#[unsafe(no_mangle)]
pub extern "C" fn asc_eq_preset_id(preset: usize) -> *const c_char {
    PRESET_IDS
        .get(preset)
        .map_or(std::ptr::null(), |id| id.as_ptr())
}

/// Writes a preset's band gains in dB; false for a bad index or a null pointer (S33).
///
/// # Safety
///
/// `gains_db` is null or points to 10 aligned, writable doubles that nothing
/// else accesses during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_eq_preset_gains(preset: usize, gains_db: *mut f64) -> bool {
    let Some(preset) = PRESETS.get(preset) else {
        return false;
    };
    if gains_db.is_null() {
        return false;
    }
    // SAFETY: the pointer is non-null and, by the caller's contract, addresses
    // BAND_COUNT aligned doubles that only this call accesses.
    let gains = unsafe { std::slice::from_raw_parts_mut(gains_db, BAND_COUNT) };
    gains.copy_from_slice(&preset.gains_db);
    true
}

/// Creates an equalizer, disabled and flat, designed for `sample_rate`; null
/// when the rate is not finite and positive.
#[unsafe(no_mangle)]
pub extern "C" fn asc_eq_create(sample_rate: f64) -> *mut AscEqualizer {
    let (mut control, processor) = eq::equalizer();
    if !control.set_sample_rate(sample_rate) {
        return std::ptr::null_mut();
    }
    Box::into_raw(Box::new(AscEqualizer { control, processor }))
}

/// Destroys an equalizer.
///
/// # Safety
///
/// `eq` is null or a pointer returned by [`asc_eq_create`] that was not
/// destroyed yet, and no other call on it runs now or later.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_eq_destroy(eq: *mut AscEqualizer) {
    if eq.is_null() {
        return;
    }
    // SAFETY: the pointer came from Box::into_raw in asc_eq_create, and by the
    // caller's contract it is destroyed once, with no call in progress.
    drop(unsafe { Box::from_raw(eq) });
}

/// Control thread: publishes new settings (S31).
///
/// # Safety
///
/// `eq` is null or a live handle, called under the threading contract (a
/// control call); `gains_db` is null or points to 10 aligned, readable doubles.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_eq_set_settings(
    eq: *mut AscEqualizer,
    enabled: bool,
    preamp_db: f64,
    gains_db: *const f64,
) -> bool {
    if eq.is_null() || gains_db.is_null() {
        return false;
    }
    // SAFETY: the pointer is non-null and, by the caller's contract, addresses
    // BAND_COUNT aligned doubles that stay unchanged during the call.
    let gains = unsafe { std::slice::from_raw_parts(gains_db, BAND_COUNT) };
    let mut settings = EqSettings {
        enabled,
        preamp_db,
        gains_db: [0.0; BAND_COUNT],
    };
    settings.gains_db.copy_from_slice(gains);
    // SAFETY: the handle is live; control calls run on one thread at a time and
    // borrow only the `control` field, which the capture thread never touches.
    let control = unsafe { &mut (*eq).control };
    control.set_settings(settings);
    true
}

/// Control thread, while capture is stopped: sets the design rate (S30).
///
/// # Safety
///
/// `eq` is null or a live handle, called under the threading contract (a
/// control call).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_eq_set_sample_rate(eq: *mut AscEqualizer, sample_rate: f64) -> bool {
    if eq.is_null() {
        return false;
    }
    // SAFETY: the handle is live; control calls run on one thread at a time and
    // borrow only the `control` field, which the capture thread never touches.
    let control = unsafe { &mut (*eq).control };
    control.set_sample_rate(sample_rate)
}

/// Capture thread: equalizes `frames` interleaved stereo frames in place (S26).
/// Real-time safe.
///
/// # Safety
///
/// `eq` is null or a live handle, called under the threading contract (the
/// capture thread); `interleaved` is null or points to `frames` × 2 aligned
/// floats, readable and writable, that nothing else accesses during the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_eq_process(
    eq: *mut AscEqualizer,
    interleaved: *mut f32,
    frames: usize,
) {
    let Some(samples) = stereo_samples(frames) else {
        return;
    };
    if eq.is_null() || interleaved.is_null() || samples == 0 {
        return;
    }
    // SAFETY: the handle is live; process calls run on one thread at a time and
    // borrow only the `processor` field, which control calls never touch.
    let processor = unsafe { &mut (*eq).processor };
    // SAFETY: the pointer is non-null and, by the caller's contract, addresses
    // `samples` aligned floats that only this call accesses.
    let block = unsafe { std::slice::from_raw_parts_mut(interleaved, samples) };
    processor.process(block);
}

/// Control thread: how many channel resets non-finite values caused (S29, S131).
///
/// # Safety
///
/// `eq` is null or a live handle, called under the threading contract (a
/// control call).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn asc_eq_nan_resets(eq: *const AscEqualizer) -> u64 {
    if eq.is_null() {
        return 0;
    }
    // SAFETY: the handle is live; the control thread reads only the `control`
    // field, which the capture thread never touches.
    let control = unsafe { &(*eq).control };
    control.nan_resets()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::f64::consts::PI;
    use std::fs;
    use std::path::Path;

    const HEADER: &str = include_str!("../include/audiosync_core.h");

    /// Every source file of the C interface, by its path under `src/`.
    const SOURCES: [(&str, &str); 13] = [
        ("ffi.rs", include_str!("ffi.rs")),
        ("ffi/airplay.rs", include_str!("ffi/airplay.rs")),
        ("ffi/calibrate.rs", include_str!("ffi/calibrate.rs")),
        ("ffi/calibrate_all.rs", include_str!("ffi/calibrate_all.rs")),
        ("ffi/cast.rs", include_str!("ffi/cast.rs")),
        ("ffi/device_volume.rs", include_str!("ffi/device_volume.rs")),
        ("ffi/presets.rs", include_str!("ffi/presets.rs")),
        ("ffi/remote.rs", include_str!("ffi/remote.rs")),
        ("ffi/render.rs", include_str!("ffi/render.rs")),
        ("ffi/ring.rs", include_str!("ffi/ring.rs")),
        ("ffi/source_quit.rs", include_str!("ffi/source_quit.rs")),
        ("ffi/sync.rs", include_str!("ffi/sync.rs")),
        ("ffi/zones.rs", include_str!("ffi/zones.rs")),
    ];

    fn sine_block(frames: usize, start: usize) -> Vec<f32> {
        (start..start + frames)
            .flat_map(|i| {
                let x = (0.25 * (2.0 * PI * 1000.0 * i as f64 / 48_000.0).sin()) as f32;
                [x, x]
            })
            .collect()
    }

    /// The C spelling of a Rust type used in this interface.
    fn c_type(rust: &str) -> String {
        let rust = rust.trim();
        if let Some(inner) = rust.strip_prefix("*mut ") {
            return format!("{}*", c_type(inner));
        }
        if let Some(inner) = rust.strip_prefix("*const ") {
            return if inner.trim_start().starts_with('*') {
                format!("{} const*", c_type(inner))
            } else {
                format!("const {}*", c_type(inner))
            };
        }
        match rust {
            "f64" => "double",
            "f32" => "float",
            "bool" => "bool",
            "usize" => "size_t",
            "u8" => "uint8_t",
            "i16" => "int16_t",
            "i32" => "int32_t",
            "u32" => "uint32_t",
            "i64" => "int64_t",
            "u64" => "uint64_t",
            "c_char" => "char",
            "c_void" => "void",
            name if name.starts_with("Asc") && name.chars().all(|c| c.is_ascii_alphanumeric()) => {
                name
            }
            other => panic!("no C spelling for the Rust type `{other}`"),
        }
        .to_owned()
    }

    fn without_whitespace(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// A Rust parameter list (`a: T, b: U,`) in C.
    fn c_params(params: &str) -> String {
        let params: Vec<String> = params
            .split(',')
            .map(str::trim)
            .filter(|param| !param.is_empty())
            .map(|param| {
                let (name, ty) = param.split_once(':').unwrap();
                format!("{} {}", c_type(ty), name.trim())
            })
            .collect();
        if params.is_empty() {
            "void".to_owned()
        } else {
            params.join(", ")
        }
    }

    /// A Rust return type (`-> T`, or nothing) in C.
    fn c_return(tail: &str) -> String {
        match tail.trim().strip_prefix("->") {
            Some(ty) => c_type(ty),
            None => "void".to_owned(),
        }
    }

    /// The lines of a source file before its first top-level test item (a
    /// function may hold a test-only statement of its own).
    fn production_lines(source: &str) -> Vec<&str> {
        source
            .lines()
            .take_while(|line| *line != "#[cfg(test)]")
            .collect()
    }

    /// `lines`, trimmed and joined by spaces, through the first line that
    /// ends with `last`.
    fn joined(lines: &[&str], last: char) -> String {
        let mut text = String::new();
        for line in lines {
            text.push_str(line.trim());
            text.push(' ');
            if line.trim_end().ends_with(last) {
                break;
            }
        }
        text
    }

    /// What one side declares for C, in C spelling without whitespace:
    /// function prototypes, structs with their fields, opaque handles and
    /// callback types.
    #[derive(Debug, Default)]
    struct Declarations {
        functions: Vec<String>,
        structs: BTreeMap<String, Vec<String>>,
        handles: BTreeSet<String>,
        callbacks: BTreeMap<String, String>,
    }

    /// The fields of the `#[repr(C)]` struct whose body starts after `lines[0]`.
    fn rust_fields(lines: &[&str]) -> Vec<String> {
        lines[1..]
            .iter()
            .map(|line| line.trim())
            .take_while(|line| *line != "}")
            .filter(|line| !line.is_empty() && !line.starts_with("//"))
            .map(|line| {
                let field = line
                    .strip_prefix("pub ")
                    .unwrap_or_else(|| panic!("a private field in a C struct: {line}"));
                let (name, ty) = field.trim_end_matches(',').split_once(':').unwrap();
                without_whitespace(&format!("{} {};", c_type(ty), name.trim()))
            })
            .collect()
    }

    /// A callback type alias, `pub type AscX = Option<unsafe extern "C"
    /// fn(...) -> T>;`, as a C typedef.
    fn rust_callback(text: &str) -> (String, String) {
        let (name, value) = text
            .strip_prefix("pub type ")
            .and_then(|rest| rest.split_once('='))
            .unwrap_or_else(|| panic!("not a type alias: {text}"));
        let name = name.trim();
        let signature = value
            .trim()
            .strip_suffix(';')
            .and_then(|value| value.trim().strip_prefix("Option<"))
            .and_then(|value| value.trim().strip_suffix('>'))
            .map(|value| value.trim().trim_end_matches(',').trim())
            .and_then(|value| value.strip_prefix("unsafe extern \"C\" fn"))
            .and_then(|value| value.trim().strip_prefix('('))
            .unwrap_or_else(|| panic!("{name} is not an optional C function pointer"));
        let (params, tail) = signature.rsplit_once(')').unwrap();
        let typedef = format!(
            "typedef {} (*{name})({});",
            c_return(tail),
            c_params(params)
        );
        (name.to_owned(), without_whitespace(&typedef))
    }

    /// Everything the Rust sources export to C.
    fn rust_declarations() -> Declarations {
        let mut found = Declarations::default();
        for (_, source) in SOURCES {
            let lines = production_lines(source);
            for (index, line) in lines.iter().enumerate() {
                let line = line.trim();
                if line == "#[unsafe(no_mangle)]" {
                    let signature = joined(&lines[index + 1..], '{');
                    assert!(
                        signature.contains("extern \"C\" fn "),
                        "not a C function: {signature}"
                    );
                    let (_, after_fn) = signature.split_once("fn ").unwrap();
                    let (name, rest) = after_fn.split_once('(').unwrap();
                    let (params, tail) = rest.rsplit_once(')').unwrap();
                    let ret = c_return(tail.trim().trim_end_matches('{'));
                    let prototype = format!("{ret} {name}({});", c_params(params));
                    found.functions.push(without_whitespace(&prototype));
                } else if let Some(rest) = line.strip_prefix("pub struct ") {
                    let name = rest.trim_end_matches('{').trim().to_owned();
                    let repr_c = lines[..index]
                        .iter()
                        .rev()
                        .map(|line| line.trim())
                        .take_while(|line| line.starts_with("#[") || line.starts_with("///"))
                        .any(|line| line == "#[repr(C)]");
                    if repr_c {
                        found.structs.insert(name, rust_fields(&lines[index..]));
                    } else {
                        found.handles.insert(name);
                    }
                } else if line.starts_with("pub type ") {
                    let (name, typedef) = rust_callback(&joined(&lines[index..], ';'));
                    found.callbacks.insert(name, typedef);
                }
            }
        }
        found.functions.sort();
        found
    }

    /// The header without comments.
    fn header_code() -> String {
        let mut code = String::new();
        let mut rest = HEADER;
        while let Some(start) = rest.find("/*") {
            code.push_str(&rest[..start]);
            let end = rest[start..].find("*/").expect("unterminated comment");
            rest = &rest[start + end + 2..];
        }
        code.push_str(rest);
        code
    }

    /// The header's statements without comments or preprocessor lines; a
    /// struct definition is one statement.
    fn header_statements() -> Vec<String> {
        let code = header_code()
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with('#') && *line != "extern \"C\" {" && *line != "}")
            .collect::<Vec<_>>()
            .join(" ");
        let mut statements = Vec::new();
        let mut current = String::new();
        let mut depth = 0usize;
        for c in code.chars() {
            match c {
                '{' => depth += 1,
                '}' => depth -= 1,
                ';' if depth == 0 => {
                    statements.push(current.trim().to_owned());
                    current.clear();
                    continue;
                }
                _ => {}
            }
            current.push(c);
        }
        assert!(
            current.trim().is_empty(),
            "unterminated statement: {current}"
        );
        statements
    }

    /// Everything the header declares.
    fn header_declarations() -> Declarations {
        let mut found = Declarations::default();
        for statement in header_statements() {
            if let Some(rest) = statement.strip_prefix("typedef struct ") {
                match rest.split_once('{') {
                    Some((name, body)) => {
                        let (fields, alias) = body.rsplit_once('}').unwrap();
                        assert_eq!(name.trim(), alias.trim(), "{statement}");
                        let fields = fields
                            .split(';')
                            .map(str::trim)
                            .filter(|field| !field.is_empty())
                            .map(|field| without_whitespace(&format!("{field};")))
                            .collect();
                        found.structs.insert(name.trim().to_owned(), fields);
                    }
                    None => {
                        let words: Vec<&str> = rest.split_whitespace().collect();
                        assert!(
                            words.len() == 2 && words[0] == words[1],
                            "not an opaque handle: {statement}"
                        );
                        found.handles.insert(words[0].to_owned());
                    }
                }
            } else if statement.starts_with("typedef ") {
                let (_, after) = statement
                    .split_once("(*")
                    .unwrap_or_else(|| panic!("not a callback type: {statement}"));
                let (name, _) = after.split_once(')').unwrap();
                found.callbacks.insert(
                    name.trim().to_owned(),
                    without_whitespace(&format!("{statement};")),
                );
            } else {
                assert!(statement.contains('('), "not a prototype: {statement}");
                found
                    .functions
                    .push(without_whitespace(&format!("{statement};")));
            }
        }
        found.functions.sort();
        found
    }

    /// The `Asc...` type names a C statement mentions (function names start
    /// with a lowercase `asc_`).
    fn type_names(statement: &str) -> Vec<&str> {
        statement
            .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .filter(|word| word.starts_with("Asc"))
            .collect()
    }

    /// The mark of a real-time call, in the header and in the Rust docs.
    const REAL_TIME: &str = "Real-time safe";

    /// `text` as its words joined by single spaces, without comment markers,
    /// so a phrase that wraps across comment lines is still found.
    fn words(text: &str) -> String {
        text.split_whitespace()
            .filter(|word| !["*", "///"].contains(word))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The C functions whose Rust doc comment marks them real-time safe.
    fn rust_real_time_calls() -> BTreeSet<String> {
        let mut marked = BTreeSet::new();
        for (_, source) in SOURCES {
            let lines = production_lines(source);
            for (index, line) in lines.iter().enumerate() {
                if line.trim() != "#[unsafe(no_mangle)]" {
                    continue;
                }
                let mut doc: Vec<&str> = lines[..index]
                    .iter()
                    .rev()
                    .map(|line| line.trim())
                    .take_while(|line| line.starts_with("///") || line.starts_with("#["))
                    .collect();
                doc.reverse();
                if words(&doc.join(" ")).contains(REAL_TIME) {
                    let signature = joined(&lines[index + 1..], '{');
                    let (_, after_fn) = signature.split_once("fn ").unwrap();
                    let (name, _) = after_fn.split_once('(').unwrap();
                    marked.insert(name.trim().to_owned());
                }
            }
        }
        marked
    }

    /// The functions whose header comment marks them real-time safe: each
    /// prototype that directly follows a comment with the mark.
    fn header_real_time_calls() -> BTreeSet<String> {
        let mut marked = BTreeSet::new();
        let mut rest = HEADER;
        while let Some(start) = rest.find("/*") {
            let end = start + rest[start..].find("*/").expect("unterminated comment");
            let comment = &rest[start + 2..end];
            rest = &rest[end + 2..];
            let code = &rest[..rest.find("/*").unwrap_or(rest.len())];
            let Some((statement, _)) = code.split_once(';') else {
                continue;
            };
            let statement = statement.trim();
            if !words(comment).contains(REAL_TIME)
                || statement.starts_with('#')
                || statement.starts_with("typedef")
            {
                continue;
            }
            let Some((head, _)) = statement.split_once('(') else {
                continue;
            };
            let name = head
                .split_whitespace()
                .last()
                .unwrap_or_default()
                .trim_start_matches('*');
            if name.starts_with("asc_") {
                marked.insert(name.to_owned());
            }
        }
        marked
    }

    fn header_defines() -> BTreeMap<String, String> {
        header_code()
            .lines()
            .filter_map(|line| {
                let rest = line.trim().strip_prefix("#define ")?;
                let (key, value) = rest.split_once(char::is_whitespace)?;
                Some((key.to_owned(), value.trim().to_owned()))
            })
            .collect()
    }

    #[test]
    fn rust_types_have_their_c_spelling() {
        assert_eq!(c_type("*const *const c_char"), "const char* const*");
        assert_eq!(c_type("*mut *const u8"), "const uint8_t**");
        assert_eq!(c_type("*const AscReader"), "const AscReader*");
        assert_eq!(c_type("*mut c_void"), "void*");
        assert_eq!(
            rust_callback(
                "pub type AscF = Option<unsafe extern \"C\" fn(a: *mut c_void, b: usize,) -> i32,>;"
            ),
            (
                "AscF".to_owned(),
                "typedefint32_t(*AscF)(void*a,size_tb);".to_owned()
            )
        );
        assert_eq!(
            type_names("AscResampleCount asc_resampler_process(const AscRing *ring, AscX*)"),
            ["AscResampleCount", "AscRing", "AscX"]
        );
    }

    #[test]
    fn every_source_of_the_interface_is_checked() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("ffi");
        let mut found: Vec<String> = fs::read_dir(&dir)
            .unwrap_or_else(|error| panic!("{}: {error}", dir.display()))
            .map(|entry| format!("ffi/{}", entry.unwrap().file_name().to_string_lossy()))
            .collect();
        found.push("ffi.rs".to_owned());
        found.sort();
        let listed: Vec<&str> = SOURCES.iter().map(|(path, _)| *path).collect();
        assert_eq!(found, listed);
    }

    #[test]
    fn the_header_declares_exactly_what_the_rust_code_exports() {
        let rust = rust_declarations();
        assert!(
            rust.functions.len() >= 60,
            "found {} exported functions",
            rust.functions.len()
        );
        assert!(rust.structs.len() >= 7, "found {:?}", rust.structs.keys());
        assert!(rust.handles.len() >= 8, "found {:?}", rust.handles);
        assert_eq!(
            rust.callbacks.keys().collect::<Vec<_>>(),
            ["AscRemoteCommand"]
        );
        let header = header_declarations();
        let out_of_date = "include/audiosync_core.h is out of date";
        assert_eq!(header.functions, rust.functions, "{out_of_date}: functions");
        assert_eq!(header.structs, rust.structs, "{out_of_date}: structs");
        assert_eq!(header.handles, rust.handles, "{out_of_date}: handles");
        assert_eq!(header.callbacks, rust.callbacks, "{out_of_date}: callbacks");
        let declared: BTreeSet<&str> = header
            .structs
            .keys()
            .chain(&header.handles)
            .chain(header.callbacks.keys())
            .map(String::as_str)
            .collect();
        for statement in header_statements() {
            for name in type_names(&statement) {
                assert!(
                    declared.contains(name),
                    "{name} is not declared: {statement}"
                );
            }
        }
    }

    // covers: S126
    #[test]
    fn the_header_and_the_rust_docs_mark_the_same_real_time_calls() {
        let header = header_real_time_calls();
        assert!(header.len() >= 13, "found {header:?}");
        assert_eq!(
            header,
            rust_real_time_calls(),
            "include/audiosync_core.h is out of date: the \"{REAL_TIME}\" marks"
        );
        // The counting-allocator proof in this file makes every marked call.
        let (_, proof) = SOURCES[0]
            .1
            .split_once("\nmod realtime {")
            .expect("the allocation proof");
        for name in &header {
            assert!(
                proof.contains(&format!("{name}(")),
                "the allocation proof never calls {name}"
            );
        }
    }

    #[test]
    fn the_header_constants_match_the_rust_ones() {
        let expected: BTreeMap<String, String> = [
            ("ASC_ABI_VERSION", ABI_VERSION.to_string()),
            ("ASC_EQ_BANDS", BAND_COUNT.to_string()),
            ("ASC_EQ_PRESETS", PRESET_COUNT.to_string()),
            (
                "ASC_RING_MARGIN_FRAMES",
                crate::ring::MIN_MARGIN_FRAMES.to_string(),
            ),
            ("ASC_RING_MIN_FRAMES", MIN_RING_FRAMES.to_string()),
            ("ASC_RING_MAX_FRAMES", MAX_RING_FRAMES.to_string()),
            ("ASC_CORRECTION_NONE", CORRECTION_NONE.to_string()),
            ("ASC_CORRECTION_PRIME", CORRECTION_PRIME.to_string()),
            ("ASC_CORRECTION_RESYNC", CORRECTION_RESYNC.to_string()),
            (
                "ASC_CORRECTION_SKIP_FORWARD",
                CORRECTION_SKIP_FORWARD.to_string(),
            ),
            ("ASC_CORRECTION_OVERRUN", CORRECTION_OVERRUN.to_string()),
            ("ASC_CORRECTION_STEP_BACK", CORRECTION_STEP_BACK.to_string()),
            ("ASC_STOP_FADE_CAP_MS", STOP_FADE_CAP_MS.to_string()),
            ("ASC_FADE_CLOSED", FADE_CLOSED.to_string()),
            ("ASC_FADE_FADING_IN", FADE_FADING_IN.to_string()),
            ("ASC_FADE_OPEN", FADE_OPEN.to_string()),
            ("ASC_FADE_FADING_OUT", FADE_FADING_OUT.to_string()),
            ("ASC_AIRPLAY_APPEARED", AIRPLAY_APPEARED.to_string()),
            ("ASC_AIRPLAY_CHANGED", AIRPLAY_CHANGED.to_string()),
            ("ASC_AIRPLAY_GONE", AIRPLAY_GONE.to_string()),
            ("ASC_AIRPLAY_AVAILABLE", AIRPLAY_AVAILABLE.to_string()),
            ("ASC_AIRPLAY_NEEDS_PIN", AIRPLAY_NEEDS_PIN.to_string()),
            (
                "ASC_AIRPLAY_NEEDS_PASSWORD",
                AIRPLAY_NEEDS_PASSWORD.to_string(),
            ),
            (
                "ASC_AIRPLAY_NEEDS_PAIRING",
                AIRPLAY_NEEDS_PAIRING.to_string(),
            ),
            (
                "ASC_AIRPLAY_AIRPLAY2_ONLY",
                AIRPLAY_AIRPLAY2_ONLY.to_string(),
            ),
            ("ASC_AIRPLAY_UNSUPPORTED", AIRPLAY_UNSUPPORTED.to_string()),
            ("ASC_AIRPLAY_REASON_NONE", AIRPLAY_REASON_NONE.to_string()),
            (
                "ASC_AIRPLAY_REASON_ENCRYPTION",
                AIRPLAY_REASON_ENCRYPTION.to_string(),
            ),
            ("ASC_AIRPLAY_REASON_CODEC", AIRPLAY_REASON_CODEC.to_string()),
            (
                "ASC_AIRPLAY_REASON_TRANSPORT",
                AIRPLAY_REASON_TRANSPORT.to_string(),
            ),
            (
                "ASC_AIRPLAY_REASON_METHODS",
                AIRPLAY_REASON_METHODS.to_string(),
            ),
            (
                "ASC_AIRPLAY_REASON_FORMAT",
                AIRPLAY_REASON_FORMAT.to_string(),
            ),
            (
                "ASC_AIRPLAY_REASON_AIRPLAY2_ONLY",
                AIRPLAY_REASON_AIRPLAY2_ONLY.to_string(),
            ),
            (
                "ASC_AIRPLAY_REASON_TIMING",
                AIRPLAY_REASON_TIMING.to_string(),
            ),
            (
                "ASC_AIRPLAY_REASON_ONE_AIRPLAY2",
                AIRPLAY_REASON_ONE_AIRPLAY2.to_string(),
            ),
            (
                "ASC_AIRPLAY_REASON_PLAYS_THROUGH_SPEAKERS",
                AIRPLAY_REASON_PLAYS_THROUGH_SPEAKERS.to_string(),
            ),
            ("ASC_AIRPLAY_FLAG_TARGET", AIRPLAY_FLAG_TARGET.to_string()),
            (
                "ASC_AIRPLAY_FLAG_INCOMPLETE",
                AIRPLAY_FLAG_INCOMPLETE.to_string(),
            ),
            ("ASC_AIRPLAY_FLAG_RAOP", AIRPLAY_FLAG_RAOP.to_string()),
            ("ASC_AIRPLAY_FLAG_AIRPLAY", AIRPLAY_FLAG_AIRPLAY.to_string()),
            (
                "ASC_AIRPLAY_FLAG_APPLE_TV",
                AIRPLAY_FLAG_APPLE_TV.to_string(),
            ),
            (
                "ASC_AIRPLAY_FLAG_PASSWORD",
                AIRPLAY_FLAG_PASSWORD.to_string(),
            ),
            ("ASC_AIRPLAY_FLAG_PAIRING", AIRPLAY_FLAG_PAIRING.to_string()),
            (
                "ASC_AIRPLAY_FLAG_LEGACY_PAIRING",
                AIRPLAY_FLAG_LEGACY_PAIRING.to_string(),
            ),
            ("ASC_AIRPLAY_FLAG_BUSY", AIRPLAY_FLAG_BUSY.to_string()),
            (
                "ASC_AIRPLAY_FLAG_AIRPLAY2",
                AIRPLAY_FLAG_AIRPLAY2.to_string(),
            ),
            (
                "ASC_AIRPLAY_FLAG_OVER_AIRPLAY2",
                AIRPLAY_FLAG_OVER_AIRPLAY2.to_string(),
            ),
            ("ASC_AIRPLAY_FLAG_PAIRED", AIRPLAY_FLAG_PAIRED.to_string()),
            ("ASC_AIRPLAY_EVENT_STATE", AIRPLAY_EVENT_STATE.to_string()),
            ("ASC_AIRPLAY_EVENT_ERROR", AIRPLAY_EVENT_ERROR.to_string()),
            (
                "ASC_AIRPLAY_EVENT_LATENCY",
                AIRPLAY_EVENT_LATENCY.to_string(),
            ),
            (
                "ASC_AIRPLAY_EVENT_TIMELINE",
                AIRPLAY_EVENT_TIMELINE.to_string(),
            ),
            ("ASC_AIRPLAY_EVENT_FAILED", AIRPLAY_EVENT_FAILED.to_string()),
            (
                "ASC_AIRPLAY_STATE_CONNECTING",
                AIRPLAY_STATE_CONNECTING.to_string(),
            ),
            (
                "ASC_AIRPLAY_STATE_PLAYING",
                AIRPLAY_STATE_PLAYING.to_string(),
            ),
            (
                "ASC_AIRPLAY_STATE_RETRYING",
                AIRPLAY_STATE_RETRYING.to_string(),
            ),
            ("ASC_AIRPLAY_STATE_FAILED", AIRPLAY_STATE_FAILED.to_string()),
            (
                "ASC_AIRPLAY_STATE_STOPPED",
                AIRPLAY_STATE_STOPPED.to_string(),
            ),
            (
                "ASC_AIRPLAY_ERROR_NOT_RESPONDING",
                AIRPLAY_ERROR_NOT_RESPONDING.to_string(),
            ),
            (
                "ASC_AIRPLAY_ERROR_CONNECTION_LOST",
                AIRPLAY_ERROR_CONNECTION_LOST.to_string(),
            ),
            ("ASC_AIRPLAY_ERROR_BUSY", AIRPLAY_ERROR_BUSY.to_string()),
            (
                "ASC_AIRPLAY_ERROR_REFUSED",
                AIRPLAY_ERROR_REFUSED.to_string(),
            ),
            (
                "ASC_AIRPLAY_ERROR_INCOMPATIBLE",
                AIRPLAY_ERROR_INCOMPATIBLE.to_string(),
            ),
            (
                "ASC_AIRPLAY_ERROR_PASSWORD_REQUIRED",
                AIRPLAY_ERROR_PASSWORD_REQUIRED.to_string(),
            ),
            (
                "ASC_AIRPLAY_ERROR_WRONG_PASSWORD",
                AIRPLAY_ERROR_WRONG_PASSWORD.to_string(),
            ),
            (
                "ASC_AIRPLAY_ERROR_PAIRING_REQUIRED",
                AIRPLAY_ERROR_PAIRING_REQUIRED.to_string(),
            ),
            (
                "ASC_AIRPLAY_ERROR_UNSUPPORTED",
                AIRPLAY_ERROR_UNSUPPORTED.to_string(),
            ),
            (
                "ASC_AIRPLAY_ERROR_WRONG_PIN",
                AIRPLAY_ERROR_WRONG_PIN.to_string(),
            ),
            (
                "ASC_AIRPLAY_EVENT_PIN_NEEDED",
                AIRPLAY_EVENT_PIN_NEEDED.to_string(),
            ),
            (
                "ASC_AIRPLAY_EVENT_PASSWORD_NEEDED",
                AIRPLAY_EVENT_PASSWORD_NEEDED.to_string(),
            ),
            (
                "ASC_AIRPLAY_EVENT_PAIRING_FAILED",
                AIRPLAY_EVENT_PAIRING_FAILED.to_string(),
            ),
            ("ASC_AIRPLAY_EVENT_PAIRED", AIRPLAY_EVENT_PAIRED.to_string()),
            ("ASC_AIRPLAY_EVENT_VOLUME", AIRPLAY_EVENT_VOLUME.to_string()),
            (
                "ASC_AIRPLAY_EVENT_WARNING",
                AIRPLAY_EVENT_WARNING.to_string(),
            ),
            ("ASC_AIRPLAY_EVENT_TRACE", AIRPLAY_EVENT_TRACE.to_string()),
            ("ASC_AIRPLAY_EVENT_REMOTE", AIRPLAY_EVENT_REMOTE.to_string()),
            (
                "ASC_AIRPLAY_REMOTE_PLAY_PAUSE",
                AIRPLAY_REMOTE_PLAY_PAUSE.to_string(),
            ),
            ("ASC_AIRPLAY_REMOTE_NEXT", AIRPLAY_REMOTE_NEXT.to_string()),
            (
                "ASC_AIRPLAY_REMOTE_PREVIOUS",
                AIRPLAY_REMOTE_PREVIOUS.to_string(),
            ),
            (
                "ASC_AIRPLAY_PLAYBACK_UNKNOWN",
                AIRPLAY_PLAYBACK_UNKNOWN.to_string(),
            ),
            (
                "ASC_AIRPLAY_PLAYBACK_PLAYING",
                AIRPLAY_PLAYBACK_PLAYING.to_string(),
            ),
            (
                "ASC_AIRPLAY_PLAYBACK_PAUSED",
                AIRPLAY_PLAYBACK_PAUSED.to_string(),
            ),
            (
                "ASC_AIRPLAY_WARNING_JACK_DISCONNECTED",
                AIRPLAY_WARNING_JACK_DISCONNECTED.to_string(),
            ),
            (
                "ASC_AIRPLAY_WARNING_AUTH_SETUP_FAILED",
                AIRPLAY_WARNING_AUTH_SETUP_FAILED.to_string(),
            ),
            (
                "ASC_AIRPLAY_WARNING_REQUEST_FAILED",
                AIRPLAY_WARNING_REQUEST_FAILED.to_string(),
            ),
            (
                "ASC_AIRPLAY_WARNING_RECEIVER_SIGNATURE_INVALID",
                AIRPLAY_WARNING_RECEIVER_SIGNATURE_INVALID.to_string(),
            ),
            (
                "ASC_AIRPLAY_WARNING_PTP_PORTS_TAKEN",
                AIRPLAY_WARNING_PTP_PORTS_TAKEN.to_string(),
            ),
            (
                "ASC_AIRPLAY_WARNING_PTP_OUTSIDE_FAMILY",
                AIRPLAY_WARNING_PTP_OUTSIDE_FAMILY.to_string(),
            ),
            (
                "ASC_AIRPLAY_WARNING_VOLUME_READS_STOPPED",
                AIRPLAY_WARNING_VOLUME_READS_STOPPED.to_string(),
            ),
            (
                "ASC_AIRPLAY_WARNING_ARTWORK_REFUSED",
                AIRPLAY_WARNING_ARTWORK_REFUSED.to_string(),
            ),
            (
                "ASC_AIRPLAY_WARNING_REMOTE_CONTROL_UNAVAILABLE",
                AIRPLAY_WARNING_REMOTE_CONTROL_UNAVAILABLE.to_string(),
            ),
            (
                "ASC_AIRPLAY_WARNING_REMOTE_CONTROL_NOT_ADVERTISED",
                AIRPLAY_WARNING_REMOTE_CONTROL_NOT_ADVERTISED.to_string(),
            ),
            (
                "ASC_AIRPLAY_WARNING_LATENCY_SHORTFALL",
                AIRPLAY_WARNING_LATENCY_SHORTFALL.to_string(),
            ),
            (
                "ASC_AIRPLAY_WARNING_PTP_FREE",
                AIRPLAY_WARNING_PTP_FREE.to_string(),
            ),
            (
                "ASC_AIRPLAY_EVENT_LATENCY_HELD",
                AIRPLAY_EVENT_LATENCY_HELD.to_string(),
            ),
            (
                "ASC_AIRPLAY_LATENCY_SOURCE_AUDIO_LATENCY",
                AIRPLAY_LATENCY_SOURCE_AUDIO_LATENCY.to_string(),
            ),
            (
                "ASC_AIRPLAY_LATENCY_SOURCE_AUDIO_LATENCIES",
                AIRPLAY_LATENCY_SOURCE_AUDIO_LATENCIES.to_string(),
            ),
            (
                "ASC_AIRPLAY_LATENCY_SOURCE_ARRIVAL_TO_RENDER",
                AIRPLAY_LATENCY_SOURCE_ARRIVAL_TO_RENDER.to_string(),
            ),
            (
                "ASC_AIRPLAY_PAIRINGS_FAILED",
                AIRPLAY_PAIRINGS_FAILED.to_string(),
            ),
            ("ASC_AIRPLAY_MAX_DELAY_MS", AIRPLAY_MAX_DELAY_MS.to_string()),
            (
                "ASC_AIRPLAY_PAIRINGS_IGNORED",
                AIRPLAY_PAIRINGS_IGNORED.to_string(),
            ),
            (
                "ASC_AIRPLAY_PAIRINGS_NEWER",
                AIRPLAY_PAIRINGS_NEWER.to_string(),
            ),
            (
                "ASC_DEVICE_VOLUME_UNKNOWN",
                DEVICE_VOLUME_UNKNOWN.to_string(),
            ),
            ("ASC_DEVICE_VOLUME_FINE", DEVICE_VOLUME_FINE.to_string()),
            ("ASC_DEVICE_VOLUME_LOW", DEVICE_VOLUME_LOW.to_string()),
            ("ASC_DEVICE_VOLUME_MUTED", DEVICE_VOLUME_MUTED.to_string()),
            (
                "ASC_DEVICE_VOLUME_MUTED_AND_LOW",
                DEVICE_VOLUME_MUTED_AND_LOW.to_string(),
            ),
            ("ASC_CALIBRATION_ACCEPTED", CALIBRATION_ACCEPTED.to_string()),
            (
                "ASC_CALIBRATION_TOO_SHORT",
                CALIBRATION_TOO_SHORT.to_string(),
            ),
            ("ASC_CALIBRATION_TOO_LONG", CALIBRATION_TOO_LONG.to_string()),
            ("ASC_CALIBRATION_CLIPPED", CALIBRATION_CLIPPED.to_string()),
            (
                "ASC_CALIBRATION_TOO_QUIET",
                CALIBRATION_TOO_QUIET.to_string(),
            ),
            (
                "ASC_CALIBRATION_AMBIGUOUS_ECHO",
                CALIBRATION_AMBIGUOUS_ECHO.to_string(),
            ),
            (
                "ASC_CALIBRATION_IMPLAUSIBLE",
                CALIBRATION_IMPLAUSIBLE.to_string(),
            ),
            (
                "ASC_CALIBRATION_RECORDING_NONE",
                CALIBRATION_RECORDING_NONE.to_string(),
            ),
            (
                "ASC_CALIBRATION_RECORDING_REFERENCE",
                CALIBRATION_RECORDING_REFERENCE.to_string(),
            ),
            (
                "ASC_CALIBRATION_RECORDING_CHOSEN",
                CALIBRATION_RECORDING_CHOSEN.to_string(),
            ),
            (
                "ASC_CALIBRATION_KIND_LOCAL",
                CALIBRATION_KIND_LOCAL.to_string(),
            ),
            (
                "ASC_CALIBRATION_KIND_CAST",
                CALIBRATION_KIND_CAST.to_string(),
            ),
            (
                "ASC_CALIBRATION_KIND_AIRPLAY",
                CALIBRATION_KIND_AIRPLAY.to_string(),
            ),
            (
                "ASC_CALIBRATION_ROW_REFERENCE",
                CALIBRATION_ROW_REFERENCE.to_string(),
            ),
            (
                "ASC_CALIBRATION_ROW_NOT_MEASURED",
                CALIBRATION_ROW_NOT_MEASURED.to_string(),
            ),
            (
                "ASC_CALIBRATION_ROW_CERTAIN",
                CALIBRATION_ROW_CERTAIN.to_string(),
            ),
            (
                "ASC_CALIBRATION_ROW_UNCERTAIN",
                CALIBRATION_ROW_UNCERTAIN.to_string(),
            ),
            (
                "ASC_CALIBRATION_ROW_REFUSED",
                CALIBRATION_ROW_REFUSED.to_string(),
            ),
            (
                "ASC_CALIBRATION_AIRPLAY_NONE_CERTAIN",
                CALIBRATION_AIRPLAY_NONE_CERTAIN.to_string(),
            ),
            (
                "ASC_CALIBRATION_AIRPLAY_CERTAIN",
                CALIBRATION_AIRPLAY_CERTAIN.to_string(),
            ),
            (
                "ASC_CALIBRATION_AIRPLAY_SPREAD",
                CALIBRATION_AIRPLAY_SPREAD.to_string(),
            ),
            ("ASC_CALIBRATION_END_NONE", CALIBRATION_END_NONE.to_string()),
            (
                "ASC_CALIBRATION_END_CANCELLED",
                CALIBRATION_END_CANCELLED.to_string(),
            ),
            (
                "ASC_CALIBRATION_END_SPEAKERS_CHANGED",
                CALIBRATION_END_SPEAKERS_CHANGED.to_string(),
            ),
            (
                "ASC_CALIBRATION_END_SPEAKER_OFF",
                CALIBRATION_END_SPEAKER_OFF.to_string(),
            ),
            (
                "ASC_CALIBRATION_END_SOURCE_CHANGED",
                CALIBRATION_END_SOURCE_CHANGED.to_string(),
            ),
            ("ASC_CALIBRATION_APPLY_OK", CALIBRATION_APPLY_OK.to_string()),
            (
                "ASC_CALIBRATION_APPLY_ENDED",
                CALIBRATION_APPLY_ENDED.to_string(),
            ),
            (
                "ASC_CALIBRATION_APPLY_NOTHING_CERTAIN",
                CALIBRATION_APPLY_NOTHING_CERTAIN.to_string(),
            ),
            (
                "ASC_CALIBRATION_APPLY_TOO_MANY",
                CALIBRATION_APPLY_TOO_MANY.to_string(),
            ),
            (
                "ASC_CALIBRATION_APPLY_INVALID",
                CALIBRATION_APPLY_INVALID.to_string(),
            ),
            (
                "ASC_CALIBRATION_APPLY_ALREADY_APPLIED",
                CALIBRATION_APPLY_ALREADY_APPLIED.to_string(),
            ),
            ("ASC_CALIBRATION_ZONE_OK", CALIBRATION_ZONE_OK.to_string()),
            (
                "ASC_CALIBRATION_ZONE_NO_REFERENCE",
                CALIBRATION_ZONE_NO_REFERENCE.to_string(),
            ),
            (
                "ASC_CALIBRATION_ZONE_NOTHING_TO_MEASURE",
                CALIBRATION_ZONE_NOTHING_TO_MEASURE.to_string(),
            ),
            (
                "ASC_CALIBRATION_ZONE_INVALID",
                CALIBRATION_ZONE_INVALID.to_string(),
            ),
            ("ASC_SOURCE_QUIT_KEEP", SOURCE_QUIT_KEEP.to_string()),
            ("ASC_SOURCE_QUIT_STOP", SOURCE_QUIT_STOP.to_string()),
            (
                "ASC_SOURCE_QUIT_SWITCH_TO_SYSTEM",
                SOURCE_QUIT_SWITCH_TO_SYSTEM.to_string(),
            ),
            ("ASC_STREAMING_OFF", STREAMING_OFF.to_string()),
            ("ASC_STREAMING_RUNNING", STREAMING_RUNNING.to_string()),
            ("ASC_STREAMING_REBUILDING", STREAMING_REBUILDING.to_string()),
            ("ASC_ZONE_MEMBER_LOCAL", ZONE_MEMBER_LOCAL.to_string()),
            ("ASC_ZONE_MEMBER_AIRPLAY", ZONE_MEMBER_AIRPLAY.to_string()),
            ("ASC_ZONE_MEMBER_AIRPLAY2", ZONE_MEMBER_AIRPLAY2.to_string()),
            ("ASC_ZONE_MEMBER_CAST", ZONE_MEMBER_CAST.to_string()),
            ("ASC_ZONE_CLICK_NOTHING", ZONE_CLICK_NOTHING.to_string()),
            ("ASC_ZONE_CLICK_TURN_ON", ZONE_CLICK_TURN_ON.to_string()),
            ("ASC_ZONE_CLICK_START", ZONE_CLICK_START.to_string()),
            ("ASC_ZONE_CLICK_TURN_OFF", ZONE_CLICK_TURN_OFF.to_string()),
            ("ASC_ZONE_STEP_ABSENT", ZONE_STEP_ABSENT.to_string()),
            (
                "ASC_ZONE_STEP_NEEDS_AIRPLAY2",
                ZONE_STEP_NEEDS_AIRPLAY2.to_string(),
            ),
            ("ASC_ZONE_STEP_IN_TARGET", ZONE_STEP_IN_TARGET.to_string()),
            ("ASC_ZONE_STEP_ON", ZONE_STEP_ON.to_string()),
            ("ASC_ZONE_STEP_WAITING", ZONE_STEP_WAITING.to_string()),
            ("ASC_ZONE_STEP_OFF", ZONE_STEP_OFF.to_string()),
            (
                "ASC_ZONE_STEP_CAST_ANOTHER_PLAYS",
                ZONE_STEP_CAST_ANOTHER_PLAYS.to_string(),
            ),
            (
                "ASC_ZONE_STEP_CAST_ONE_AT_A_TIME",
                ZONE_STEP_CAST_ONE_AT_A_TIME.to_string(),
            ),
            ("ASC_ZONE_STEP_CAST_BUSY", ZONE_STEP_CAST_BUSY.to_string()),
            (
                "ASC_PRESET_RECALL_NOTHING",
                PRESET_RECALL_NOTHING.to_string(),
            ),
            ("ASC_PRESET_RECALL_APPLY", PRESET_RECALL_APPLY.to_string()),
            ("ASC_PRESET_RECALL_START", PRESET_RECALL_START.to_string()),
            ("ASC_PRESET_STEP_ON", PRESET_STEP_ON.to_string()),
            ("ASC_PRESET_STEP_WAITING", PRESET_STEP_WAITING.to_string()),
            (
                "ASC_PRESET_STEP_NOT_FOUND",
                PRESET_STEP_NOT_FOUND.to_string(),
            ),
            ("ASC_PRESET_STEP_HIDDEN", PRESET_STEP_HIDDEN.to_string()),
            (
                "ASC_PRESET_STEP_NEEDS_AIRPLAY2",
                PRESET_STEP_NEEDS_AIRPLAY2.to_string(),
            ),
            (
                "ASC_PRESET_STEP_IN_TARGET",
                PRESET_STEP_IN_TARGET.to_string(),
            ),
            ("ASC_PRESET_STEP_BUSY", PRESET_STEP_BUSY.to_string()),
            ("ASC_PRESET_STEP_IGNORED", PRESET_STEP_IGNORED.to_string()),
            ("ASC_METER_POLL_MS", crate::meter::METER_POLL_MS.to_string()),
            ("ASC_CAST_RATE", CAST_RATE.to_string()),
            ("ASC_CAST_APPEARED", CAST_APPEARED.to_string()),
            ("ASC_CAST_CHANGED", CAST_CHANGED.to_string()),
            ("ASC_CAST_GONE", CAST_GONE.to_string()),
            ("ASC_CAST_FLAG_GROUP", CAST_FLAG_GROUP.to_string()),
            ("ASC_CAST_FLAG_OFFLINE", CAST_FLAG_OFFLINE.to_string()),
            ("ASC_CAST_FLAG_MANUAL", CAST_FLAG_MANUAL.to_string()),
            ("ASC_CAST_REFUSED_NONE", CAST_REFUSED_NONE.to_string()),
            ("ASC_CAST_REFUSED_BUSY", CAST_REFUSED_BUSY.to_string()),
            ("ASC_CAST_REFUSED_UNKNOWN", CAST_REFUSED_UNKNOWN.to_string()),
            ("ASC_CAST_REFUSED_GROUP", CAST_REFUSED_GROUP.to_string()),
            ("ASC_CAST_REFUSED_CLOSED", CAST_REFUSED_CLOSED.to_string()),
            ("ASC_CAST_REFUSED_INVALID", CAST_REFUSED_INVALID.to_string()),
            ("ASC_CAST_EVENT_STARTED", CAST_EVENT_STARTED.to_string()),
            ("ASC_CAST_EVENT_ENDED", CAST_EVENT_ENDED.to_string()),
            ("ASC_CAST_EVENT_PLAYING", CAST_EVENT_PLAYING.to_string()),
            (
                "ASC_CAST_EVENT_CONNECT_FAILED",
                CAST_EVENT_CONNECT_FAILED.to_string(),
            ),
            (
                "ASC_CAST_EVENT_RECOVERING",
                CAST_EVENT_RECOVERING.to_string(),
            ),
            ("ASC_CAST_EVENT_RECOVERED", CAST_EVENT_RECOVERED.to_string()),
            ("ASC_CAST_EVENT_RELOADED", CAST_EVENT_RELOADED.to_string()),
            (
                "ASC_CAST_EVENT_REBUFFERING",
                CAST_EVENT_REBUFFERING.to_string(),
            ),
            (
                "ASC_CAST_EVENT_FORMAT_FALLBACK",
                CAST_EVENT_FORMAT_FALLBACK.to_string(),
            ),
            ("ASC_CAST_EVENT_VOLUME", CAST_EVENT_VOLUME.to_string()),
            (
                "ASC_CAST_EVENT_DIAGNOSTICS",
                CAST_EVENT_DIAGNOSTICS.to_string(),
            ),
            ("ASC_CAST_EVENT_STREAM", CAST_EVENT_STREAM.to_string()),
            (
                "ASC_CAST_EVENT_COMMAND_FAILED",
                CAST_EVENT_COMMAND_FAILED.to_string(),
            ),
            (
                "ASC_CAST_EVENT_MEMORY_CHANGED",
                CAST_EVENT_MEMORY_CHANGED.to_string(),
            ),
            (
                "ASC_CAST_EVENT_DISCOVERY_STOPPED",
                CAST_EVENT_DISCOVERY_STOPPED.to_string(),
            ),
            ("ASC_CAST_EVENT_FAILED", CAST_EVENT_FAILED.to_string()),
            ("ASC_CAST_EVENT_MEMBERS", CAST_EVENT_MEMBERS.to_string()),
            ("ASC_CAST_EVENT_GROUP_LOG", CAST_EVENT_GROUP_LOG.to_string()),
            ("ASC_CAST_END_STOPPED", CAST_END_STOPPED.to_string()),
            ("ASC_CAST_END_TAKEN_OVER", CAST_END_TAKEN_OVER.to_string()),
            ("ASC_CAST_END_DESELECTED", CAST_END_DESELECTED.to_string()),
            ("ASC_CAST_END_FAILED", CAST_END_FAILED.to_string()),
            ("ASC_CAST_ERROR_NONE", CAST_ERROR_NONE.to_string()),
            ("ASC_CAST_ERROR_CONNECT", CAST_ERROR_CONNECT.to_string()),
            ("ASC_CAST_ERROR_LAUNCH", CAST_ERROR_LAUNCH.to_string()),
            ("ASC_CAST_ERROR_MEDIA", CAST_ERROR_MEDIA.to_string()),
            ("ASC_CAST_ERROR_NO_FETCH", CAST_ERROR_NO_FETCH.to_string()),
            ("ASC_CAST_ERROR_STREAM", CAST_ERROR_STREAM.to_string()),
            (
                "ASC_CAST_ERROR_NO_SESSION",
                CAST_ERROR_NO_SESSION.to_string(),
            ),
            (
                "ASC_CAST_ERROR_VOLUME_FIXED",
                CAST_ERROR_VOLUME_FIXED.to_string(),
            ),
            ("ASC_CAST_ERROR_VOLUME", CAST_ERROR_VOLUME.to_string()),
            ("ASC_CAST_ERROR_ARTWORK", CAST_ERROR_ARTWORK.to_string()),
            ("ASC_CAST_ERROR_GROUP", CAST_ERROR_GROUP.to_string()),
            ("ASC_CAST_FORMAT_WAV", CAST_FORMAT_WAV.to_string()),
            ("ASC_CAST_FORMAT_FLAC", CAST_FORMAT_FLAC.to_string()),
            ("ASC_REMOTE_DEFAULT_PORT", REMOTE_DEFAULT_PORT.to_string()),
            ("ASC_REMOTE_ERROR_NONE", REMOTE_ERROR_NONE.to_string()),
            ("ASC_REMOTE_ERROR_CONFIG", REMOTE_ERROR_CONFIG.to_string()),
            (
                "ASC_REMOTE_ERROR_PORT_IN_USE",
                REMOTE_ERROR_PORT_IN_USE.to_string(),
            ),
            ("ASC_REMOTE_ERROR_LISTEN", REMOTE_ERROR_LISTEN.to_string()),
            ("ASC_REMOTE_ERROR_RANDOM", REMOTE_ERROR_RANDOM.to_string()),
            ("ASC_REMOTE_ERROR_FAILED", REMOTE_ERROR_FAILED.to_string()),
        ]
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect();
        assert_eq!(
            header_defines(),
            expected,
            "include/audiosync_core.h is out of date: constants"
        );
    }

    #[test]
    fn version_and_volume_curve_cross_the_boundary() {
        // SAFETY: asc_version returns a static NUL-terminated string.
        let version = unsafe { CStr::from_ptr(asc_version()) };
        assert_eq!(version.to_str(), Ok(crate::VERSION));
        assert_eq!(asc_abi_version(), ABI_VERSION);
        assert_eq!(asc_volume_gain(50.0), 0.125);
        assert_eq!(asc_volume_gain(0.0), 0.0);
        assert_eq!(asc_volume_gain(100.0), 1.0);
        assert!((asc_volume_position(0.125) - 50.0).abs() < 1e-9);
    }

    // covers: S71
    #[test]
    fn an_outputs_gain_is_its_own_times_the_masters() {
        assert_eq!(asc_output_gain(50.0, 100.0), 0.125);
        assert_eq!(asc_output_gain(100.0, 0.0), 0.0);
        assert_eq!(asc_output_gain(60.0, 50.0), volume::output_gain(60.0, 50.0));
        assert_eq!(
            asc_output_gain(60.0, 50.0),
            asc_volume_gain(60.0) * asc_volume_gain(50.0)
        );
        assert_eq!(asc_output_gain(f64::NAN, 100.0), 0.0);
        assert_eq!(asc_output_gain(150.0, 100.0), 1.0);
    }

    // covers: S71, S249
    #[test]
    fn a_zoned_gain_crosses_the_boundary_as_the_core_makes_it() {
        assert_eq!(asc_zone_output_gain(50.0, 50.0, 100.0), 0.125 * 0.125);
        for (local, master) in [(60.0, 50.0), (37.0, 81.0), (0.0, 100.0)] {
            assert_eq!(
                asc_zone_output_gain(local, 100.0, master).to_bits(),
                asc_output_gain(local, master).to_bits(),
                "zone 100 is no zone"
            );
        }
        assert_eq!(asc_zone_output_gain(100.0, 0.0, 100.0), 0.0);
    }

    // covers: S70
    #[test]
    fn negative_zero_crosses_the_boundary_as_zero() {
        let positive_zero = 0.0f64.to_bits();
        assert_eq!(asc_volume_gain(-0.0).to_bits(), positive_zero);
        assert_eq!(asc_volume_position(-0.0).to_bits(), positive_zero);
    }

    // covers: S33
    #[test]
    fn presets_cross_the_boundary_in_menu_order() {
        assert_eq!(asc_eq_preset_count(), PRESETS.len());
        for (index, preset) in PRESETS.iter().enumerate() {
            // SAFETY: a valid index gives a static NUL-terminated string.
            let id = unsafe { CStr::from_ptr(asc_eq_preset_id(index)) };
            assert_eq!(id.to_str(), Ok(preset.id));
            let mut gains = [f64::NAN; BAND_COUNT];
            // SAFETY: `gains` holds BAND_COUNT doubles.
            assert!(unsafe { asc_eq_preset_gains(index, gains.as_mut_ptr()) });
            assert_eq!(gains, preset.gains_db);
        }
        assert!(asc_eq_preset_id(PRESETS.len()).is_null());
        let mut gains = [0.0; BAND_COUNT];
        // SAFETY: `gains` holds BAND_COUNT doubles; the index is out of range.
        assert!(!unsafe { asc_eq_preset_gains(PRESETS.len(), gains.as_mut_ptr()) });
        // SAFETY: a null pointer is allowed.
        assert!(!unsafe { asc_eq_preset_gains(0, std::ptr::null_mut()) });
        assert_eq!(asc_eq_band_count(), BAND_COUNT);
        assert_eq!(asc_eq_band_center_hz(5), 1000.0);
        assert_eq!(asc_eq_band_center_hz(BAND_COUNT), 0.0);
    }

    // covers: S2, S26
    #[test]
    fn an_equalizer_through_the_c_interface_matches_the_rust_one() {
        let eq = asc_eq_create(44_100.0);
        assert!(!eq.is_null());
        let mut gains = PRESETS[1].gains_db;
        gains[5] = 6.0;
        // SAFETY: `eq` is live and `gains` holds BAND_COUNT doubles.
        assert!(unsafe { asc_eq_set_settings(eq, true, -1.5, gains.as_ptr()) });
        let (mut control, mut reference) = eq::equalizer();
        control.set_sample_rate(44_100.0);
        control.set_settings(EqSettings {
            enabled: true,
            preamp_db: -1.5,
            gains_db: gains,
        });
        for start in (0..8192).step_by(512) {
            let mut through_c = sine_block(512, start);
            let mut direct = through_c.clone();
            // SAFETY: `eq` is live and the block holds 512 stereo frames.
            unsafe { asc_eq_process(eq, through_c.as_mut_ptr(), 512) };
            reference.process(&mut direct);
            assert_eq!(through_c, direct);
        }
        // SAFETY: `eq` is live.
        assert!(unsafe { asc_eq_set_sample_rate(eq, 96_000.0) });
        // SAFETY: `eq` is live.
        assert!(!unsafe { asc_eq_set_sample_rate(eq, f64::NAN) });
        // SAFETY: `eq` is live; it is destroyed once.
        unsafe { asc_eq_destroy(eq) };
    }

    // covers: S29, S131
    #[test]
    fn the_c_interface_reports_non_finite_resets() {
        let eq = asc_eq_create(48_000.0);
        let gains = [0.0; BAND_COUNT];
        // SAFETY: `eq` is live and `gains` holds BAND_COUNT doubles.
        unsafe { asc_eq_set_settings(eq, true, 0.0, gains.as_ptr()) };
        let mut block = sine_block(64, 0);
        block[10] = f32::NAN;
        block[21] = f32::INFINITY;
        // SAFETY: `eq` is live and the block holds 64 stereo frames.
        unsafe { asc_eq_process(eq, block.as_mut_ptr(), 64) };
        assert!(block.iter().all(|x| x.is_finite()));
        // SAFETY: `eq` is live.
        assert_eq!(unsafe { asc_eq_nan_resets(eq) }, 2);
        // SAFETY: `eq` is live; it is destroyed once.
        unsafe { asc_eq_destroy(eq) };
    }

    #[test]
    fn null_pointers_and_bad_arguments_are_harmless() {
        assert!(asc_eq_create(0.0).is_null());
        assert!(asc_eq_create(f64::INFINITY).is_null());
        let gains = [0.0; BAND_COUNT];
        let mut block = sine_block(4, 0);
        let copy = block.clone();
        // SAFETY: every call below gets a null handle or a null buffer, which the
        // interface allows.
        unsafe {
            asc_eq_destroy(std::ptr::null_mut());
            assert!(!asc_eq_set_settings(
                std::ptr::null_mut(),
                true,
                0.0,
                gains.as_ptr()
            ));
            assert!(!asc_eq_set_sample_rate(std::ptr::null_mut(), 48_000.0));
            asc_eq_process(std::ptr::null_mut(), block.as_mut_ptr(), 4);
            assert_eq!(asc_eq_nan_resets(std::ptr::null()), 0);
            asc_apply_gain(std::ptr::null_mut(), 4, 0.5);
        }
        assert_eq!(block, copy);
        let eq = asc_eq_create(48_000.0);
        // SAFETY: `eq` is live; the buffers are null or hold 4 frames.
        unsafe {
            assert!(!asc_eq_set_settings(eq, true, 0.0, std::ptr::null()));
            asc_eq_process(eq, std::ptr::null_mut(), 4);
            asc_eq_process(eq, block.as_mut_ptr(), 0);
            asc_eq_process(eq, block.as_mut_ptr(), usize::MAX);
            asc_eq_destroy(eq);
        }
        assert_eq!(block, copy);
    }

    // covers: S64, S65
    #[test]
    fn gain_crosses_the_boundary_a_buffer_at_a_time() {
        let mut block = vec![0.5f32, -0.5, f32::NAN, f32::INFINITY, 1.0, -1.0];
        // SAFETY: the block holds 3 stereo frames.
        unsafe { asc_apply_gain(block.as_mut_ptr(), 3, 0.5) };
        assert_eq!(block, [0.25, -0.25, 0.0, 0.0, 0.5, -0.5]);
        // SAFETY: the block holds 3 stereo frames.
        unsafe { asc_apply_gain(block.as_mut_ptr(), 3, 0.0) };
        assert!(block.iter().all(|x| x.to_bits() == 0));
    }
}

/// The real-time check: a counting global allocator for this crate's unit
/// tests. It lives here so that every `unsafe` line of the crate is in this file.
#[cfg(test)]
mod realtime {
    use super::*;
    use crate::volume::{GainStage, SharedGain};
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    thread_local! {
        static ARMED: Cell<bool> = const { Cell::new(false) };
        static EVENTS: Cell<usize> = const { Cell::new(0) };
    }

    /// Counts allocations and frees made by an armed thread.
    struct CountingAllocator;

    fn note() {
        // try_with: the allocator may run while a thread's locals are torn down.
        let _ = ARMED.try_with(|armed| {
            if armed.get() {
                let _ = EVENTS.try_with(|events| events.set(events.get() + 1));
            }
        });
    }

    // SAFETY: every method forwards its arguments unchanged to the system
    // allocator, which upholds the GlobalAlloc contract; the bookkeeping touches
    // only const-initialized thread-locals without destructors, which neither
    // allocate nor unwind.
    unsafe impl GlobalAlloc for CountingAllocator {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            note();
            // SAFETY: forwarded unchanged; the caller upholds `alloc`'s contract.
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            note();
            // SAFETY: forwarded unchanged; the caller upholds `alloc_zeroed`'s contract.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            note();
            // SAFETY: forwarded unchanged; the caller upholds `dealloc`'s contract.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            note();
            // SAFETY: forwarded unchanged; the caller upholds `realloc`'s contract.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    #[global_allocator]
    static ALLOCATOR: CountingAllocator = CountingAllocator;

    /// Allocations plus frees the current thread makes while running `f`.
    fn heap_events(f: impl FnOnce()) -> usize {
        EVENTS.with(|events| events.set(0));
        ARMED.with(|armed| armed.set(true));
        f();
        ARMED.with(|armed| armed.set(false));
        EVENTS.with(Cell::get)
    }

    #[test]
    fn the_counter_sees_heap_use() {
        let events = heap_events(|| {
            let boxed = std::hint::black_box(Box::new([0u8; 64]));
            drop(boxed);
        });
        assert_eq!(events, 2);
    }

    // The AirPlay spec's P6 asks the same of the sender's input.
    // covers: S126
    #[test]
    fn airplay_input_calls_never_touch_the_heap() {
        // Setup may allocate: the sender, its input, the block.
        let mut sender = asc_airplay_create();
        assert!(!sender.is_null());
        // SAFETY: a live sender handle, used by this thread only.
        let mut input = unsafe { asc_airplay_take_input(sender) };
        assert!(!input.is_null());
        let block: Vec<f32> = (0..512 * 2).map(|i| (i % 17) as f32 / 34.0).collect();
        let mut handed = 0usize;
        let events = heap_events(|| {
            for round in 0..2000usize {
                // SAFETY: a live input, written by this thread only; `block`
                // holds 512 stereo frames. A rate request is an any-thread
                // call; each one makes the next write start its resampler
                // afresh at that rate, which must not allocate either.
                unsafe {
                    if round % 250 == 0 {
                        let rate = [48_000.0, 44_100.0, 96_000.0, 22_050.0][round / 250 % 4];
                        assert!(asc_airplay_input_set_rate(input, rate));
                    }
                    // A stop's fade-out, then zeros until the next rate
                    // request fades in again (S189).
                    if round % 250 == 125 {
                        asc_airplay_input_request_fade_out(input);
                    }
                    handed += asc_airplay_input_write(input, block.as_ptr(), 512);
                }
            }
        });
        // SAFETY: each handle destroyed once, by this thread, with no call
        // in progress.
        unsafe {
            asc_airplay_input_destroy(&mut input);
            asc_airplay_destroy(&mut sender);
        }
        assert_eq!(
            events, 0,
            "a write or a rate request allocated or freed memory"
        );
        assert!(handed > 0);
    }

    // The Cast spec's C8 asks the same of the sender's input.
    // covers: S126
    #[test]
    fn cast_input_calls_never_touch_the_heap() {
        // Setup may allocate: the sender, its input, a session's ring.
        // SAFETY: a null memory text starts empty.
        let mut cast = unsafe { asc_cast_create(std::ptr::null()) };
        assert!(!cast.is_null());
        // SAFETY: a live handle, used by this thread only; the strings are
        // NUL-terminated literals.
        let mut input = unsafe {
            // Nothing listens on port 9 of the loopback: the session
            // retries its connect while the input writes into its ring.
            assert!(asc_cast_add_device(cast, c"127.0.0.1:9".as_ptr()));
            assert!(asc_cast_start(cast, c"manual:127.0.0.1:9".as_ptr(), std::ptr::null_mut()) > 0);
            asc_cast_take_input(cast)
        };
        assert!(!input.is_null());
        let block: Vec<f32> = (0..512 * 2).map(|i| (i % 17) as f32 / 34.0).collect();
        let mut taken = 0usize;
        let events = heap_events(|| {
            for round in 0..400usize {
                // SAFETY: a live input, written by this thread only; `block`
                // holds 512 stereo frames.
                unsafe {
                    if round % 100 == 0 {
                        let rate = [48_000.0, 44_100.0, 96_000.0, 22_050.0][round / 100 % 4];
                        assert!(asc_cast_input_set_rate(input, rate));
                    }
                    if round % 100 == 50 {
                        asc_cast_input_request_fade_out(input);
                    }
                    taken += asc_cast_input_write(input, block.as_ptr(), 512);
                }
            }
        });
        // SAFETY: each handle destroyed once, by this thread, with no call
        // in progress.
        unsafe {
            asc_cast_input_destroy(&mut input);
            asc_cast_destroy(&mut cast);
        }
        assert_eq!(
            events, 0,
            "a write or a rate request allocated or freed memory"
        );
        assert!(taken > 0, "the session's ring took nothing");
    }

    // covers: S126, S128, S130
    #[test]
    fn equalizer_and_gain_paths_never_touch_the_heap() {
        // Setup may allocate: the halves, the buffers, the C handle.
        let (mut control, mut processor) = eq::equalizer();
        control.set_settings(EqSettings {
            enabled: true,
            preamp_db: -3.0,
            gains_db: PRESETS[10].gains_db,
        });
        let handle = asc_eq_create(48_000.0);
        let gains = PRESETS[3].gains_db;
        // SAFETY: `handle` is live and `gains` holds BAND_COUNT doubles.
        unsafe { asc_eq_set_settings(handle, true, 0.0, gains.as_ptr()) };
        let mut block = vec![0.0f32; 1024];
        let mut device = vec![0.0f32; 512 * 4];
        let quad_input: Vec<f32> = (0..512 * 4).map(|i| (i % 13) as f32 / 26.0).collect();
        let shared_gain = Arc::new(SharedGain::new(0.5));
        let mut stage = GainStage::new();
        let mut ring_sum = 0.0f32;
        let mut airplay_sum = 0.0f32;

        // The control thread keeps publishing while the capture thread runs.
        let stop = Arc::new(AtomicBool::new(false));
        let publisher = {
            let stop = Arc::clone(&stop);
            let shared_gain = Arc::clone(&shared_gain);
            std::thread::spawn(move || {
                let mut round = 0usize;
                while !stop.load(Ordering::Relaxed) {
                    let preset = &PRESETS[round % PRESETS.len()];
                    control.set_settings(EqSettings {
                        enabled: round % 5 != 0,
                        preamp_db: -((round % 7) as f64),
                        gains_db: preset.gains_db,
                    });
                    shared_gain.set((round % 4) as f32 / 4.0);
                    round += 1;
                    std::thread::yield_now();
                }
            })
        };

        let events = heap_events(|| {
            for round in 0..2000 {
                for (index, sample) in block.iter_mut().enumerate() {
                    *sample = (((round * 512 + index / 2) % 97) as f32 / 97.0 - 0.5) * 0.5;
                }
                if round % 50 == 0 {
                    block[7] = f32::NAN;
                    block[8] = f32::NEG_INFINITY;
                }
                processor.equalize_and_deliver(
                    &mut block,
                    |samples| ring_sum += samples[0],
                    |samples| airplay_sum += samples[1],
                );
                // SAFETY: `handle` is live and `block` holds 512 stereo frames.
                unsafe { asc_eq_process(handle, block.as_mut_ptr(), 512) };
                device.copy_from_slice(&quad_input);
                stage.apply(&mut device, 4, shared_gain.get());
                crate::channels::input_to_stereo(&quad_input, 4, &mut block);
                // SAFETY: `block` holds 512 stereo frames.
                unsafe { asc_apply_gain(block.as_mut_ptr(), 512, 0.25) };
                crate::volume::apply_gain(&mut block, 0.0);
            }
        });

        stop.store(true, Ordering::Relaxed);
        publisher.join().unwrap();
        // SAFETY: `handle` is live; it is destroyed once.
        unsafe { asc_eq_destroy(handle) };
        assert_eq!(events, 0, "the audio path allocated or freed memory");
        assert!(ring_sum.is_finite() && airplay_sum.is_finite());
    }

    /// Interleaved stereo test audio: a quiet saw on both channels.
    fn saw(frames: usize) -> Vec<f32> {
        (0..frames * 2)
            .map(|i| ((i / 2) % 100) as f32 / 400.0 - 0.125)
            .collect()
    }

    // covers: S126, S127, S128, S133
    #[test]
    fn every_real_time_call_of_the_c_interface_never_touches_the_heap() {
        // Setup may allocate: the handles and the buffers.
        let ring = asc_ring_create(asc_ring_capacity_for_rate(48_000));
        // SAFETY: `ring` is live.
        let writer = unsafe { asc_ring_writer_create(ring) };
        // A session start resets the ring before any reader exists (S39).
        // SAFETY: `writer` is live and no reader exists yet.
        let reset = heap_events(|| unsafe { asc_ring_writer_reset(writer) });
        // SAFETY: `ring` is live.
        let (near, far) = unsafe {
            (
                asc_reader_create(ring, 48_000),
                asc_reader_create(ring, 48_000),
            )
        };
        let resampler = asc_resampler_create(48_000, 44_100);
        let stage = asc_gain_stage_create(0.5);
        // SAFETY: `ring` is live.
        let (converting, timed) = unsafe {
            (
                asc_renderer_create(ring, 48_000, 44_100, 6, 0.5),
                asc_renderer_create(ring, 48_000, 48_000, 2, 1.0),
            )
        };
        let mut rendered = vec![0.0f32; 1024 * 6];
        let mut fade_end = 0u64;
        let eq = asc_eq_create(48_000.0);
        let gains = PRESETS[1].gains_db;
        // SAFETY: `eq` is live and `gains` holds BAND_COUNT doubles.
        unsafe { asc_eq_set_settings(eq, true, -2.0, gains.as_ptr()) };
        let mut capture = saw(512);
        capture[6] = f32::NAN;
        capture[7] = f32::INFINITY;
        let large = saw(20_000);
        let mut block = vec![0.0f32; 512 * 2];
        let mut out = vec![0.0f32; 8192 * 2];
        let mut converted = vec![0.0f32; 1024 * 2];
        let mut device = vec![0.0f32; 512 * 6];
        let mut report = AscReadReport::default();

        let events = heap_events(|| {
            for round in 0..600usize {
                // SAFETY: every handle is live and used under its threading
                // contract (all from this one thread); every buffer holds the
                // frames passed with it.
                unsafe {
                    // The capture thread; it pauses for a while now and then.
                    block.copy_from_slice(&capture);
                    asc_eq_process(eq, block.as_mut_ptr(), 512);
                    if round % 97 == 13 {
                        asc_ring_write(writer, large.as_ptr(), 20_000);
                    } else if !(20..25).contains(&(round % 50)) {
                        asc_ring_write(writer, block.as_ptr(), 512);
                    }
                    // Any thread: the atomic control calls and the counters.
                    if round % 37 == 0 {
                        asc_reader_set_delay_frames(near, 4096 + round as i64);
                        asc_reader_set_tolerance_frames(far, 1680);
                        asc_reader_request_resync(far);
                        asc_gain_stage_set_gain(stage, (round % 4) as f32 / 4.0);
                    }
                    if round % 151 == 0 {
                        asc_reader_reset_prime(near);
                    }
                    std::hint::black_box((
                        asc_reader_delay_frames(near),
                        asc_reader_tolerance_frames(far),
                        asc_reader_lag_frames(near),
                        asc_reader_counters(far),
                        asc_gain_stage_gain(stage),
                        asc_ring_frames_written(ring),
                        asc_ring_capacity(ring),
                        asc_ring_supports_rate(ring, 44_100),
                    ));
                    // An output at the capture rate with six channels.
                    asc_reader_read(near, out.as_mut_ptr(), 512, &mut report);
                    asc_gain_stage_render(stage, out.as_ptr(), 512, device.as_mut_ptr(), 6);
                    // Two renderers with their fades: one converting to
                    // 44.1 kHz with six channels, one timed; delay changes
                    // fade, and the timed one fades out for a stop.
                    if round % 41 == 0 {
                        asc_renderer_set_delay_frames(converting, 2048 + (round % 7) as i64 * 512);
                        asc_renderer_set_gain(converting, (round % 3) as f32 / 2.0);
                    }
                    if round == 400 {
                        asc_renderer_request_fade_out(timed);
                    }
                    asc_renderer_render(converting, rendered.as_mut_ptr(), 441 + round % 300);
                    asc_renderer_render_timed(
                        timed,
                        rendered.as_mut_ptr(),
                        512,
                        1_000_000_000 + round as u64 * 10_666_667,
                    );
                    std::hint::black_box((
                        asc_renderer_gain(converting),
                        asc_renderer_delay_frames(converting),
                        asc_renderer_fade_rendered(timed),
                        asc_renderer_fade_end_time_ns(timed, &mut fade_end),
                        asc_renderer_faded_by_time(timed, 5_000_000, 2_000_000_000),
                        asc_renderer_fade_state(converting),
                        asc_renderer_fade_frames(converting),
                        asc_renderer_lag_frames(timed),
                        asc_renderer_counters(converting),
                    ));
                    // An output at 44.1 kHz, fed through the resampler,
                    // which now and then starts a new stream.
                    if round % 89 == 0 {
                        asc_resampler_reset(resampler);
                    }
                    std::hint::black_box(asc_resampler_latency_frames(resampler));
                    let need = asc_resampler_input_frames_for(resampler, 441);
                    asc_reader_read_at(
                        far,
                        out.as_mut_ptr(),
                        need,
                        9000,
                        1680,
                        std::ptr::null_mut(),
                    );
                    let made = asc_resampler_process(
                        resampler,
                        out.as_ptr(),
                        need,
                        converted.as_mut_ptr(),
                        1024,
                    );
                    asc_gain_stage_apply(stage, converted.as_mut_ptr(), made.output_frames, 2);
                    asc_apply_gain(converted.as_mut_ptr(), made.output_frames, 0.5);
                }
            }
        });

        // SAFETY: every handle is live; each is destroyed once.
        let (near_counters, far_counters) = unsafe {
            let counters = (asc_reader_counters(near), asc_reader_counters(far));
            asc_eq_destroy(eq);
            asc_gain_stage_destroy(stage);
            asc_renderer_destroy(converting);
            asc_renderer_destroy(timed);
            asc_resampler_destroy(resampler);
            asc_reader_destroy(near);
            asc_reader_destroy(far);
            asc_ring_writer_destroy(writer);
            asc_ring_destroy(ring);
            counters
        };
        assert_eq!(reset, 0, "the ring reset allocated or freed memory");
        assert_eq!(events, 0, "a real-time call allocated or freed memory");
        assert_eq!(report.frames, 512);
        assert!(near_counters.primes > 1 && near_counters.resyncs > 0);
        assert!(far_counters.drift_skips + far_counters.drift_silences > 0);
        assert!(
            fade_end > 1_000_000_000,
            "the timed renderer published its fade's end"
        );
    }

    /// A handle that test threads share; each thread makes only the calls
    /// that the handle's threading contract gives it.
    struct Handle<T>(*mut T);

    impl<T> Clone for Handle<T> {
        fn clone(&self) -> Self {
            *self
        }
    }

    impl<T> Copy for Handle<T> {}

    // SAFETY: the pointer is only passed to the C interface, by threads that
    // follow the handle's threading contract.
    unsafe impl<T> Send for Handle<T> {}

    impl<T> Handle<T> {
        /// The pointer. A method, so a closure captures the whole Handle.
        fn get(self) -> *mut T {
            self.0
        }
    }

    // covers: S126, S128, S132, S133
    #[test]
    fn audio_threads_and_a_control_thread_share_handles_without_the_heap() {
        const ROUNDS: usize = 3000;
        let ring = asc_ring_create(asc_ring_capacity_for_rate(48_000));
        // SAFETY: `ring` is live.
        let (writer, near, far) = unsafe {
            (
                asc_ring_writer_create(ring),
                asc_reader_create(ring, 48_000),
                asc_reader_create(ring, 48_000),
            )
        };
        let near_stage = asc_gain_stage_create(1.0);
        let far_stage = asc_gain_stage_create(1.0);
        // SAFETY: `ring` is live.
        let renderer = unsafe { asc_renderer_create(ring, 48_000, 44_100, 2, 1.0) };
        let renderer_h = Handle(renderer);
        let resampler = asc_resampler_create(48_000, 44_100);
        let eq = asc_eq_create(48_000.0);
        let (ring_h, writer_h, near_h, far_h) =
            (Handle(ring), Handle(writer), Handle(near), Handle(far));
        let (near_stage_h, far_stage_h, resampler_h, eq_h) = (
            Handle(near_stage),
            Handle(far_stage),
            Handle(resampler),
            Handle(eq),
        );

        let events: Vec<usize> = std::thread::scope(|scope| {
            let capture = scope.spawn(move || {
                let mut block = saw(512);
                heap_events(|| {
                    for _ in 0..ROUNDS {
                        // SAFETY: this thread makes every capture-side call
                        // on these handles; `block` holds 512 frames.
                        unsafe {
                            asc_eq_process(eq_h.get(), block.as_mut_ptr(), 512);
                            asc_ring_write(writer_h.get(), block.as_ptr(), 512);
                        }
                    }
                })
            });
            let near_output = scope.spawn(move || {
                let mut out = vec![0.0f32; 512 * 2];
                let mut device = vec![0.0f32; 512 * 2];
                heap_events(|| {
                    for _ in 0..ROUNDS {
                        // SAFETY: this thread makes every read of `near`,
                        // every output-side call of its stage and every render
                        // of `renderer`.
                        unsafe {
                            asc_reader_read(
                                near_h.get(),
                                out.as_mut_ptr(),
                                512,
                                std::ptr::null_mut(),
                            );
                            asc_gain_stage_render(
                                near_stage_h.get(),
                                out.as_ptr(),
                                512,
                                device.as_mut_ptr(),
                                2,
                            );
                            asc_renderer_render_timed(
                                renderer_h.get(),
                                device.as_mut_ptr(),
                                512,
                                2_000_000_000,
                            );
                        }
                    }
                })
            });
            let far_output = scope.spawn(move || {
                let mut out = vec![0.0f32; 2048 * 2];
                let mut converted = vec![0.0f32; 1024 * 2];
                heap_events(|| {
                    for round in 0..ROUNDS {
                        // SAFETY: this thread makes every read of `far`, every
                        // resampler call and every output-side call of its stage.
                        unsafe {
                            if round % 500 == 0 {
                                asc_resampler_reset(resampler_h.get());
                            }
                            std::hint::black_box(asc_resampler_latency_frames(resampler_h.get()));
                            let need = asc_resampler_input_frames_for(resampler_h.get(), 441);
                            asc_reader_read(
                                far_h.get(),
                                out.as_mut_ptr(),
                                need,
                                std::ptr::null_mut(),
                            );
                            let made = asc_resampler_process(
                                resampler_h.get(),
                                out.as_ptr(),
                                need,
                                converted.as_mut_ptr(),
                                1024,
                            );
                            asc_gain_stage_apply(
                                far_stage_h.get(),
                                converted.as_mut_ptr(),
                                made.output_frames,
                                2,
                            );
                        }
                    }
                })
            });
            let control = scope.spawn(move || {
                let gains = PRESETS[4].gains_db;
                heap_events(|| {
                    for round in 0..ROUNDS {
                        // SAFETY: every call here is an any-thread call, or an
                        // equalizer control call, which only this thread makes.
                        unsafe {
                            if round % 100 == 0 {
                                asc_reader_set_delay_frames(near_h.get(), 2048 + round as i64);
                                asc_reader_request_resync(far_h.get());
                                asc_gain_stage_set_gain(
                                    near_stage_h.get(),
                                    (round % 3) as f32 / 2.0,
                                );
                                asc_gain_stage_set_gain(far_stage_h.get(), 0.25);
                                asc_renderer_set_delay_frames(
                                    renderer_h.get(),
                                    1024 + round as i64,
                                );
                                asc_renderer_set_gain(renderer_h.get(), (round % 4) as f32 / 4.0);
                                asc_eq_set_settings(
                                    eq_h.get(),
                                    round % 200 == 0,
                                    -1.0,
                                    gains.as_ptr(),
                                );
                            }
                            if round % 777 == 0 {
                                asc_reader_reset_prime(far_h.get());
                            }
                            if round == ROUNDS / 2 {
                                asc_renderer_request_fade_out(renderer_h.get());
                            }
                            let mut end = 0u64;
                            std::hint::black_box((
                                asc_renderer_fade_end_time_ns(renderer_h.get(), &mut end),
                                asc_renderer_faded_by_time(renderer_h.get(), 1_000, 3_000_000_000),
                                asc_renderer_fade_state(renderer_h.get()),
                                asc_renderer_lag_frames(renderer_h.get()),
                                asc_renderer_counters(renderer_h.get()),
                            ));
                            std::hint::black_box((
                                asc_reader_lag_frames(near_h.get()),
                                asc_reader_counters(far_h.get()),
                                asc_reader_delay_frames(near_h.get()),
                                asc_ring_frames_written(ring_h.get()),
                                asc_gain_stage_gain(far_stage_h.get()),
                                asc_eq_nan_resets(eq_h.get()),
                            ));
                        }
                    }
                })
            });
            [capture, near_output, far_output, control]
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect()
        });

        // SAFETY: every thread has finished; each handle is destroyed once.
        let written = unsafe {
            let written = asc_ring_frames_written(ring);
            asc_eq_destroy(eq);
            asc_resampler_destroy(resampler);
            asc_gain_stage_destroy(near_stage);
            asc_gain_stage_destroy(far_stage);
            asc_renderer_destroy(renderer);
            asc_reader_destroy(near);
            asc_reader_destroy(far);
            asc_ring_writer_destroy(writer);
            asc_ring_destroy(ring);
            written
        };
        assert_eq!(events, [0; 4], "a thread allocated or freed memory");
        assert_eq!(written, (ROUNDS * 512) as u64);
    }
}
