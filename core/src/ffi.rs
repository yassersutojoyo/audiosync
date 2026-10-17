//! The C interface: a plain C ABI, declared by the hand-written
//! header `include/audiosync_core.h`.
//!
//! Audio crosses the boundary a whole buffer at a time: a pointer to
//! interleaved stereo float32 frames plus a frame count, never per sample. The
//! audio-path calls (`asc_eq_process`, `asc_apply_gain`) never allocate, lock,
//! wait or make a system call (S126).
//!
//! Threading contract of an equalizer handle (S113, S132): `asc_eq_process`
//! runs on one capture thread at a time; every other `asc_eq_*` call on the
//! handle runs on one control thread at a time; `asc_eq_destroy` runs once no
//! other call on the handle can run. The two threads touch different fields of
//! the handle ([`EqProcessor`] and [`EqControl`]), which share only atomics.
//!
//! This is the crate's only module with `unsafe` code; each block states why it
//! is sound.

#![warn(clippy::undocumented_unsafe_blocks)]

use std::ffi::{CStr, c_char};

use crate::eq::{
    self, BAND_CENTERS_HZ, BAND_COUNT, EqControl, EqProcessor, EqSettings, PRESET_COUNT, PRESETS,
};
use crate::volume;

/// Version of this C interface, bumped on any incompatible change; the header's
/// `ASC_ABI_VERSION`.
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

/// The number of samples in `frames` stereo frames, if such a buffer can exist.
fn stereo_samples(frames: usize) -> Option<usize> {
    frames
        .checked_mul(2)
        .filter(|&samples| samples <= isize::MAX as usize / size_of::<f32>())
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

/// [`volume::apply_gain`] on `frames` interleaved stereo frames, in place (S64, S65).
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
    use std::f64::consts::PI;

    const HEADER: &str = include_str!("../include/audiosync_core.h");
    const SOURCE: &str = include_str!("ffi.rs");

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
        if let Some(inner) = rust.strip_prefix("*mut ") {
            return format!("{}*", c_type(inner));
        }
        if let Some(inner) = rust.strip_prefix("*const ") {
            return format!("const {}*", c_type(inner));
        }
        match rust {
            "f64" => "double",
            "f32" => "float",
            "bool" => "bool",
            "usize" => "size_t",
            "u32" => "uint32_t",
            "u64" => "uint64_t",
            "c_char" => "char",
            "AscEqualizer" => "AscEqualizer",
            other => panic!("no C spelling for the Rust type `{other}`"),
        }
        .to_owned()
    }

    fn without_whitespace(text: &str) -> String {
        text.chars().filter(|c| !c.is_whitespace()).collect()
    }

    /// The C prototype of every exported function, from this file's Rust source.
    fn prototypes_from_rust() -> Vec<String> {
        let lines: Vec<&str> = SOURCE.lines().collect();
        let mut prototypes = Vec::new();
        for (index, line) in lines.iter().enumerate() {
            if line.trim() != "#[unsafe(no_mangle)]" {
                continue;
            }
            let mut signature = String::new();
            for next in &lines[index + 1..] {
                signature.push_str(next.trim());
                signature.push(' ');
                if next.trim_end().ends_with('{') {
                    break;
                }
            }
            assert!(
                signature.contains("extern \"C\" fn "),
                "not a C function: {signature}"
            );
            let (_, after_fn) = signature.split_once("fn ").unwrap();
            let (name, rest) = after_fn.split_once('(').unwrap();
            let (params, tail) = rest.rsplit_once(')').unwrap();
            let tail = tail.trim().trim_end_matches('{').trim();
            let ret = match tail.strip_prefix("->") {
                Some(ty) => c_type(ty.trim()),
                None => "void".to_owned(),
            };
            let params: Vec<String> = params
                .split(',')
                .map(str::trim)
                .filter(|param| !param.is_empty())
                .map(|param| {
                    let (param_name, ty) = param.split_once(':').unwrap();
                    format!("{} {}", c_type(ty.trim()), param_name.trim())
                })
                .collect();
            let params = if params.is_empty() {
                "void".to_owned()
            } else {
                params.join(", ")
            };
            prototypes.push(without_whitespace(&format!("{ret} {name}({params});")));
        }
        prototypes.sort();
        prototypes
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

    /// Every function prototype the header declares.
    fn prototypes_from_header() -> Vec<String> {
        let declarations: String = header_code()
            .lines()
            .map(str::trim)
            .filter(|line| !line.starts_with('#') && *line != "extern \"C\" {" && *line != "}")
            .collect::<Vec<_>>()
            .join(" ");
        let mut prototypes: Vec<String> = declarations
            .split(';')
            .map(str::trim)
            .filter(|statement| statement.contains('(') && !statement.starts_with("typedef"))
            .map(|statement| without_whitespace(&format!("{statement};")))
            .collect();
        prototypes.sort();
        prototypes
    }

    fn header_define(name: &str) -> Option<String> {
        header_code().lines().find_map(|line| {
            let rest = line.trim().strip_prefix("#define ")?;
            let (key, value) = rest.split_once(char::is_whitespace)?;
            (key == name).then(|| value.trim().to_owned())
        })
    }

    #[test]
    fn the_header_declares_exactly_the_exported_functions() {
        let from_rust = prototypes_from_rust();
        let from_header = prototypes_from_header();
        assert!(
            from_rust.len() >= 15,
            "found {} exported functions",
            from_rust.len()
        );
        assert_eq!(
            from_header, from_rust,
            "include/audiosync_core.h is out of date"
        );
        assert_eq!(
            header_define("ASC_ABI_VERSION"),
            Some(ABI_VERSION.to_string())
        );
        assert_eq!(header_define("ASC_EQ_BANDS"), Some(BAND_COUNT.to_string()));
        assert_eq!(
            header_define("ASC_EQ_PRESETS"),
            Some(PRESET_COUNT.to_string())
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
        let mut fifo_sum = 0.0f32;

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
                    |samples| fifo_sum += samples[1],
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
        assert!(ring_sum.is_finite() && fifo_sum.is_finite());
    }
}
