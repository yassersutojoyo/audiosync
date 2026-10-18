/*
 * audiosync_core.h: the C interface of audiosync-core, AudioSync's
 * platform-independent audio core (MIT).
 *
 * Audio crosses this interface one whole buffer at a time: a pointer to
 * interleaved stereo float32 frames (L, R) plus a frame count, never one
 * sample per call. The audio-path calls, asc_eq_process and asc_apply_gain,
 * never allocate, lock, wait or make a system call.
 *
 * Threading of an equalizer handle: asc_eq_process runs on one capture thread
 * at a time; every other asc_eq_* call on the handle runs on one control
 * thread at a time (the two may run concurrently); asc_eq_destroy runs once no
 * other call on the handle can run. All other functions are thread-safe.
 *
 * Every pointer argument may be NULL, which makes the call do nothing (or
 * return false, 0 or NULL). A buffer argument otherwise points to the stated
 * number of aligned, valid elements that nothing else touches during the call.
 *
 * This header is written by hand; a test in core/src/ffi.rs fails when it no
 * longer matches the Rust definitions.
 */

#ifndef AUDIOSYNC_CORE_H
#define AUDIOSYNC_CORE_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Version of this interface; asc_abi_version() returns the library's. */
#define ASC_ABI_VERSION 1

/* Number of equalizer bands. */
#define ASC_EQ_BANDS 10

/* Number of equalizer presets. */
#define ASC_EQ_PRESETS 12

/* The crate version, for example "0.1.0" (static string, never freed). */
const char *asc_version(void);

/* The interface version the library implements; compare with ASC_ABI_VERSION. */
uint32_t asc_abi_version(void);

/* Linear gain of a volume slider position 0...100: (p / 100)^3, clamped. */
double asc_volume_gain(double position);

/* Volume slider position of a linear gain 0...1: 100 * cbrt(g), clamped. */
double asc_volume_position(double gain);

/*
 * Multiplies frames x 2 interleaved samples by gain, in place. Gain 0 (or a
 * negative or non-finite gain) gives exact zeros; any NaN or infinity in the
 * result becomes 0. Real-time safe.
 */
void asc_apply_gain(float *interleaved, size_t frames, float gain);

/* Number of equalizer bands (ASC_EQ_BANDS). */
size_t asc_eq_band_count(void);

/* Center frequency of a band in Hz, ascending from band 0; 0 for a bad index. */
double asc_eq_band_center_hz(size_t band);

/* Number of presets (ASC_EQ_PRESETS). */
size_t asc_eq_preset_count(void);

/* Identifier of a preset in menu order, for example "bassBoost"; NULL for a bad index. */
const char *asc_eq_preset_id(size_t preset);

/* Writes a preset's ASC_EQ_BANDS band gains (dB) to gains_db; false for a bad index. */
bool asc_eq_preset_gains(size_t preset, double *gains_db);

/* An equalizer instance; create, use and destroy it through the calls below. */
typedef struct AscEqualizer AscEqualizer;

/*
 * Creates an equalizer, disabled and flat, designed for sample_rate (Hz).
 * Returns NULL when the rate is not finite and positive.
 */
AscEqualizer *asc_eq_create(double sample_rate);

/* Destroys an equalizer. */
void asc_eq_destroy(AscEqualizer *eq);

/*
 * Control thread: publishes new settings (gains_db holds ASC_EQ_BANDS values),
 * taking effect at the next processed block, all at once. Non-finite values
 * count as 0 dB. Returns false when eq or gains_db is NULL.
 */
bool asc_eq_set_settings(AscEqualizer *eq, bool enabled, double preamp_db, const double *gains_db);

/*
 * Control thread, while capture is stopped: sets the design rate (Hz),
 * recomputing the filters and clearing their state; the same rate again does
 * nothing. Returns false for a NULL handle or a rate that isn't finite and positive.
 */
bool asc_eq_set_sample_rate(AscEqualizer *eq, double sample_rate);

/* Capture thread: equalizes frames x 2 interleaved samples in place. Real-time safe. */
void asc_eq_process(AscEqualizer *eq, float *interleaved, size_t frames);

/* Control thread: how often a channel produced NaN or infinity and was reset. */
uint64_t asc_eq_nan_resets(const AscEqualizer *eq);

#ifdef __cplusplus
}
#endif

#endif /* AUDIOSYNC_CORE_H */
