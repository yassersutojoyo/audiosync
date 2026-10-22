/*
 * audiosync_core.h: the C interface of audiosync-core, AudioSync's
 * platform-independent audio core.
 *
 * Copyright (C) 2026 Yasser Sutojoyo Kusumo
 * SPDX-License-Identifier: GPL-3.0-or-later
 *
 * Audio crosses this interface one whole buffer at a time: a pointer to
 * interleaved stereo float32 frames (L, R) plus a frame count, never one
 * sample per call. The core owns no thread, device or file: the host
 * captures, plays and runs every thread, and calls in here.
 * The exceptions are AudioSync's own AirPlay and Google Cast senders
 * (asc_airplay_*, asc_cast_*) and its web remote (asc_remote_*), which run
 * their own threads and network sockets.
 *
 * Real-time calls (marked "Real-time safe") never allocate, free, lock, wait,
 * log or make a system call, and never panic: bad arguments make them do
 * nothing or saturate.
 *
 * Threads. Each handle lists which thread runs which call:
 * - an audio call (capture thread or output thread) runs on one thread at a
 *   time per handle;
 * - a control call runs on one control thread at a time per handle, and may
 *   run while that handle's audio call runs;
 * - an "any thread" call is a lock-free atomic operation and may run anywhere,
 *   at any time, while the handle lives;
 * - a destroy call runs once no other call on that handle can run.
 * Handles that share audio (a ring and its writer and readers) are
 * reference-counted inside, so they may be destroyed in any order. Functions
 * without a handle are thread-safe.
 *
 * Pointers. A handle argument may be NULL, which makes the call do nothing
 * and return false, 0, NULL or an all-zero struct, unless its comment says
 * otherwise. A buffer argument points to the stated number of aligned, valid
 * elements that nothing else touches during the call, and the buffers of one
 * call don't overlap. A buffer may be NULL when that number is 0, and a NULL
 * buffer with a nonzero count makes the call do nothing.
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

/* The ring's safety margin M in frames: no ring holds fewer than twice this. */
#define ASC_RING_MARGIN_FRAMES 8192

/* Smallest ring capacity in frames (twice the margin). */
#define ASC_RING_MIN_FRAMES 16384

/* Largest ring capacity in frames: 24 s at 384 kHz. */
#define ASC_RING_MAX_FRAMES 9216000

/* How a read re-positioned its reader (AscReadReport.correction). */
#define ASC_CORRECTION_NONE 0         /* the block follows the previous one */
#define ASC_CORRECTION_PRIME 1        /* first anchor, or the first after a prime reset */
#define ASC_CORRECTION_RESYNC 2       /* a requested resync, e.g. a delay change */
#define ASC_CORRECTION_SKIP_FORWARD 3 /* behind by more than the tolerance */
#define ASC_CORRECTION_OVERRUN 4      /* the frames it needed were overwritten */
#define ASC_CORRECTION_STEP_BACK 5    /* ahead by more than the tolerance */

/* What an AirPlay receiver event reports (AscAirPlayReceiver.event). */
#define ASC_AIRPLAY_APPEARED 1 /* discovery found a receiver */
#define ASC_AIRPLAY_CHANGED 2  /* its records or its verdict changed */
#define ASC_AIRPLAY_GONE 3     /* it left the network, or discovery stopped */

/* Whether a receiver can play from this sender (AscAirPlayReceiver.availability). */
#define ASC_AIRPLAY_AVAILABLE 0      /* it can */
#define ASC_AIRPLAY_NEEDS_PIN 1      /* it asks for a PIN once: an Apple TV, or a receiver reached over AirPlay 2 */
#define ASC_AIRPLAY_NEEDS_PASSWORD 2 /* it asks for a password */
#define ASC_AIRPLAY_NEEDS_PAIRING 3  /* it likely needs another kind of pairing */
#define ASC_AIRPLAY_AIRPLAY2_ONLY 4  /* only AirPlay 2 could serve it, and that path is off */
#define ASC_AIRPLAY_UNSUPPORTED 5    /* the sender can't drive it; .unsupported says why */

/* Why a receiver can't play (AscAirPlayReceiver.unsupported). */
#define ASC_AIRPLAY_REASON_NONE 0          /* none: it can, or another reason */
#define ASC_AIRPLAY_REASON_ENCRYPTION 1    /* no encryption mode the sender supports */
#define ASC_AIRPLAY_REASON_CODEC 2         /* no codec the sender may use */
#define ASC_AIRPLAY_REASON_TRANSPORT 3     /* no UDP transport */
#define ASC_AIRPLAY_REASON_METHODS 4       /* no ANNOUNCE, SETUP or RECORD */
#define ASC_AIRPLAY_REASON_FORMAT 5        /* not 44 100 Hz, 16-bit stereo */
#define ASC_AIRPLAY_REASON_AIRPLAY2_ONLY 6 /* only AirPlay 2 could serve it */
#define ASC_AIRPLAY_REASON_TIMING 7        /* it refused NTP timing, and PTP can't take its place */
#define ASC_AIRPLAY_REASON_ONE_AIRPLAY2 8  /* another AirPlay 2 speaker plays; one at a time */
#define ASC_AIRPLAY_REASON_PLAYS_THROUGH_SPEAKERS 9 /* an Apple TV whose home theater group plays, or the group of one that plays alone */

/* What discovery saw (AscAirPlayReceiver.flags), bit by bit. */
#define ASC_AIRPLAY_FLAG_RAOP 1            /* announces _raop._tcp (AirPlay 1 audio) */
#define ASC_AIRPLAY_FLAG_AIRPLAY 2         /* announces _airplay._tcp */
#define ASC_AIRPLAY_FLAG_APPLE_TV 4        /* its model is an Apple TV */
#define ASC_AIRPLAY_FLAG_PASSWORD 8        /* its records say it needs a password */
#define ASC_AIRPLAY_FLAG_PAIRING 16        /* its flags hint that it needs pairing */
#define ASC_AIRPLAY_FLAG_LEGACY_PAIRING 32 /* an Apple TV that needs the PIN pairing */
#define ASC_AIRPLAY_FLAG_BUSY 64           /* its flags hint that another sender plays */
#define ASC_AIRPLAY_FLAG_AIRPLAY2 128      /* it lists AirPlay 2 features */
#define ASC_AIRPLAY_FLAG_OVER_AIRPLAY2 256 /* the AirPlay 2 switch is on and the sender reaches it over AirPlay 2 */
#define ASC_AIRPLAY_FLAG_PAIRED 512        /* credentials are kept for it, which asc_airplay_forget_pairing drops */
#define ASC_AIRPLAY_FLAG_TARGET 1024       /* a stereo pair or home theater group, added like a speaker; only with the AirPlay 2 switch on */
#define ASC_AIRPLAY_FLAG_INCOMPLETE 2048   /* a target with a member out of view, playing nothing, or reporting its partner unreachable */

/* What a sender event reports (AscAirPlayEvent.kind). */
#define ASC_AIRPLAY_EVENT_STATE 1    /* a speaker's state changed: .state */
#define ASC_AIRPLAY_EVENT_ERROR 2    /* an error ended a speaker's session: .error */
#define ASC_AIRPLAY_EVENT_LATENCY 3  /* a speaker's latencies: .latency_ms, .receiver_latency_frames */
#define ASC_AIRPLAY_EVENT_TIMELINE 4 /* the base latency L or the timeline changed: .latency_ms */
#define ASC_AIRPLAY_EVENT_FAILED 5   /* the sender or its discovery stopped working (a bug); destroy it, create a new one; .detail where a panic was, or "" */
#define ASC_AIRPLAY_EVENT_PIN_NEEDED 6      /* a pairing's Apple TV shows a PIN: ask the user, then asc_airplay_submit_pin */
#define ASC_AIRPLAY_EVENT_PASSWORD_NEEDED 7 /* a pairing needs the password (.error PASSWORD_REQUIRED) or got a wrong one (WRONG_PASSWORD, forgotten) */
#define ASC_AIRPLAY_EVENT_PAIRING_FAILED 8  /* a pairing ended without credentials: .error (WRONG_PIN, NOT_RESPONDING, ...) */
#define ASC_AIRPLAY_EVENT_PAIRED 9          /* paired, credentials stored; .detail "" or why they last this run only */

/* A speaker's state (AscAirPlayEvent.state). */
#define ASC_AIRPLAY_STATE_CONNECTING 1 /* a session is starting */
#define ASC_AIRPLAY_STATE_PLAYING 2    /* it plays the shared timeline */
#define ASC_AIRPLAY_STATE_RETRYING 3   /* its session failed; the next starts in .retry_ms */
#define ASC_AIRPLAY_STATE_FAILED 4     /* its session failed and needs the user; no retry */
#define ASC_AIRPLAY_STATE_STOPPED 5    /* removed, or the sender shut down */

/* What to tell the user about an error that ended a session (AscAirPlayEvent.error). */
#define ASC_AIRPLAY_ERROR_NOT_RESPONDING 1    /* the speaker didn't answer */
#define ASC_AIRPLAY_ERROR_CONNECTION_LOST 2   /* the connection died while playing */
#define ASC_AIRPLAY_ERROR_BUSY 3              /* another sender seems to play to it */
#define ASC_AIRPLAY_ERROR_REFUSED 4           /* it turned the session down */
#define ASC_AIRPLAY_ERROR_INCOMPATIBLE 5      /* it answered in a way the sender doesn't understand */
#define ASC_AIRPLAY_ERROR_PASSWORD_REQUIRED 6 /* it needs a password and none was given */
#define ASC_AIRPLAY_ERROR_WRONG_PASSWORD 7    /* it rejected the password */
#define ASC_AIRPLAY_ERROR_PAIRING_REQUIRED 8  /* it needs pairing, or rejected or cancelled it */
#define ASC_AIRPLAY_ERROR_UNSUPPORTED 9       /* the sender can't drive it; .unsupported says why */
#define ASC_AIRPLAY_ERROR_WRONG_PIN 10        /* a pairing's Apple TV turned the PIN down; a new pairing shows a new one */

/* What asc_airplay_open_pairings returns when the store holds nothing it found. */
#define ASC_AIRPLAY_PAIRINGS_FAILED -1  /* NULL or dead handle, or NULL, empty or non-UTF-8 path */
#define ASC_AIRPLAY_PAIRINGS_IGNORED -2 /* unreadable or not in the format: ignored, the next pairing replaces it */
#define ASC_AIRPLAY_PAIRINGS_NEWER -3   /* a newer AudioSync wrote it: ignored and left as it is */

/* The largest hold-back asc_airplay_set_hold_back applies, in ms: the sender's limit on a speaker's delay. */
#define ASC_AIRPLAY_MAX_DELAY_MS 20000

/* The rate of every Cast stream; the Cast input resamples the capture to it. */
#define ASC_CAST_RATE 48000

/* What a Cast device change reports (AscCastDevice.event). */
#define ASC_CAST_APPEARED 1 /* discovery found it, or the host added it by address */
#define ASC_CAST_CHANGED 2  /* its records changed, or it went offline during a session */
#define ASC_CAST_GONE 3     /* it left the list */

/* What a Cast device is (AscCastDevice.flags), bit by bit. */
#define ASC_CAST_FLAG_GROUP 1   /* a Cast group: plays as one device, with its own volume and offset */
#define ASC_CAST_FLAG_OFFLINE 2 /* its records went away while a session with it runs */
#define ASC_CAST_FLAG_MANUAL 4  /* added by address; no browse has found its id yet */

/* Why asc_cast_start refused (its *refused). */
#define ASC_CAST_REFUSED_NONE 0    /* it didn't */
#define ASC_CAST_REFUSED_BUSY 1    /* another Cast device plays or starts: one at a time */
#define ASC_CAST_REFUSED_UNKNOWN 2 /* the id is not a listed device */
#define ASC_CAST_REFUSED_GROUP 3   /* no longer reported: a Cast group starts like a device */
#define ASC_CAST_REFUSED_CLOSED 4  /* NULL or dead handle */
#define ASC_CAST_REFUSED_INVALID 5 /* NULL or non-UTF-8 id */

/* What a Cast event reports (AscCastEvent.kind). */
#define ASC_CAST_EVENT_STARTED 1           /* the device plays the stream: .format */
#define ASC_CAST_EVENT_ENDED 2             /* the attempt is over, its last event: .reason, and .error when it failed */
#define ASC_CAST_EVENT_PLAYING 3           /* the device reports PLAYING */
#define ASC_CAST_EVENT_CONNECT_FAILED 4    /* a connect attempt failed: .error, the next in .retry_ms */
#define ASC_CAST_EVENT_RECOVERING 5        /* the channel failed and the sender reconnects */
#define ASC_CAST_EVENT_RECOVERED 6         /* the session carries on: .relaunched */
#define ASC_CAST_EVENT_RELOADED 7          /* the device saw the stream end; it was loaded again */
#define ASC_CAST_EVENT_REBUFFERING 8       /* the receiver ran dry */
#define ASC_CAST_EVENT_FORMAT_FALLBACK 9   /* the device refused WAV; .format is loaded instead */
#define ASC_CAST_EVENT_VOLUME 10           /* the device's volume: .level, .muted, .volume_fixed, .step */
#define ASC_CAST_EVENT_DIAGNOSTICS 11      /* once a second while playing: .drift_ms, .queued_ms, .resync_offered */
#define ASC_CAST_EVENT_STREAM 12           /* the stream server's log line in .detail */
#define ASC_CAST_EVENT_COMMAND_FAILED 13   /* a volume, artwork or Resync request failed: .error */
#define ASC_CAST_EVENT_MEMORY_CHANGED 14   /* store asc_cast_memory_json */
#define ASC_CAST_EVENT_DISCOVERY_STOPPED 15 /* discovery's mDNS daemon ended; its devices were reported gone */
#define ASC_CAST_EVENT_FAILED 16           /* the sender stopped working (a bug); destroy it, create a new one; .detail where a panic was, or "" */
#define ASC_CAST_EVENT_MEMBERS 17          /* the playing group's members changed: asc_cast_member_count, asc_cast_member */
#define ASC_CAST_EVENT_GROUP_LOG 18        /* a line about a Cast group for the debug log in .detail */

/* How a Cast attempt ended (AscCastEvent.reason). */
#define ASC_CAST_END_STOPPED 1     /* the sender's own teardown */
#define ASC_CAST_END_TAKEN_OVER 2  /* someone else stopped or replaced the app or the stream */
#define ASC_CAST_END_DESELECTED 3  /* stopped before it started */
#define ASC_CAST_END_FAILED 4      /* a failure the session could not recover from: .error */

/* Why something Cast failed (AscCastEvent.error). */
#define ASC_CAST_ERROR_NONE 0
#define ASC_CAST_ERROR_CONNECT 1      /* no TCP connection, a TLS failure, or no status in time */
#define ASC_CAST_ERROR_LAUNCH 2       /* the Default Media Receiver did not launch */
#define ASC_CAST_ERROR_MEDIA 3        /* the device refused or stopped the stream */
#define ASC_CAST_ERROR_NO_FETCH 4     /* the device never fetched the stream: name a firewall as the likely cause */
#define ASC_CAST_ERROR_STREAM 5       /* the stream server could not start */
#define ASC_CAST_ERROR_NO_SESSION 6   /* no session runs */
#define ASC_CAST_ERROR_VOLUME_FIXED 7 /* the device's volume is locked */
#define ASC_CAST_ERROR_VOLUME 8       /* the device was not ready, or has no volume step */
#define ASC_CAST_ERROR_ARTWORK 9      /* artwork that is not PNG or JPEG, or over 1280 by 720 */
#define ASC_CAST_ERROR_GROUP 10       /* no longer reported: a Cast group plays like a device */

/* A Cast stream's format (AscCastEvent.format). */
#define ASC_CAST_FORMAT_WAV 1
#define ASC_CAST_FORMAT_FLAC 2

/* What an output device's own volume and mute mean for its row (AscDeviceVolumeDecision.decision). */
#define ASC_DEVICE_VOLUME_UNKNOWN 0       /* neither part is known: no hint */
#define ASC_DEVICE_VOLUME_FINE 1          /* not muted, not low: no hint */
#define ASC_DEVICE_VOLUME_LOW 2           /* the volume is 25 % or less */
#define ASC_DEVICE_VOLUME_MUTED 3         /* muted, at a volume that isn't low or isn't known */
#define ASC_DEVICE_VOLUME_MUTED_AND_LOW 4 /* muted, and 25 % or less too */

/* What asc_calibration_estimate found (AscCalibration.status). */
#define ASC_CALIBRATION_ACCEPTED 0       /* offset_ms is the new offset */
#define ASC_CALIBRATION_TOO_SHORT 1      /* under the signal plus 100 ms, or the signal isn't the rate's */
#define ASC_CALIBRATION_TOO_LONG 2       /* over 2^21 frames */
#define ASC_CALIBRATION_CLIPPED 3        /* 3 samples in a row at 0.99 or more, or one not finite */
#define ASC_CALIBRATION_TOO_QUIET 4      /* no peak 12 times over the noise floor */
#define ASC_CALIBRATION_AMBIGUOUS_ECHO 5 /* a second arrival over 0.7 of the first, 3 ms or more away */
#define ASC_CALIBRATION_IMPLAUSIBLE 6    /* a difference over 22 000 ms */

/* Which recording a refusal concerns (AscCalibration.recording). */
#define ASC_CALIBRATION_RECORDING_NONE 0      /* accepted, or a refusal about the pair */
#define ASC_CALIBRATION_RECORDING_REFERENCE 1 /* the reference's recording */
#define ASC_CALIBRATION_RECORDING_CHOSEN 2    /* the chosen output's recording */

/* How a speaker's delay is kept (AscCalibrationSpeaker.kind). */
#define ASC_CALIBRATION_KIND_LOCAL 0   /* a local output's offset, -2000...+20 000 ms */
#define ASC_CALIBRATION_KIND_CAST 1    /* a Cast speaker's measured offset, kept to 0...20 000 ms (the sender stores up to 120 000) */
#define ASC_CALIBRATION_KIND_AIRPLAY 2 /* the one AirPlay delay as shown, 0...20 000 ms */

/* What the sheet says about a speaker (AscCalibrationRow.state). */
#define ASC_CALIBRATION_ROW_REFERENCE 0    /* the reference, never measured */
#define ASC_CALIBRATION_ROW_NOT_MEASURED 1 /* the run ended before its turn */
#define ASC_CALIBRATION_ROW_CERTAIN 2      /* new_ms applies */
#define ASC_CALIBRATION_ROW_UNCERTAIN 3    /* uncertainty over 10 ms: shown, never applied */
#define ASC_CALIBRATION_ROW_REFUSED 4      /* a sweep was refused: reason and recording say why */

/* What the shared AirPlay delay comes to (AscCalibrationAirPlay.state). */
#define ASC_CALIBRATION_AIRPLAY_NONE_CERTAIN 0 /* no AirPlay row is certain: the delay stays */
#define ASC_CALIBRATION_AIRPLAY_CERTAIN 1      /* the median applies */
#define ASC_CALIBRATION_AIRPLAY_SPREAD 2       /* the median applies, but the speakers differ by spread_ms, over 10 ms */

/* Why a run or a sheet ended (asc_calibration_run_cancel, asc_calibration_sheet_ended). */
#define ASC_CALIBRATION_END_NONE 0             /* it goes on */
#define ASC_CALIBRATION_END_CANCELLED 1        /* the user cancelled */
#define ASC_CALIBRATION_END_SPEAKERS_CHANGED 2 /* the speaker list changed */
#define ASC_CALIBRATION_END_SPEAKER_OFF 3      /* a speaker taking part was turned off */
#define ASC_CALIBRATION_END_SOURCE_CHANGED 4   /* the source was switched */

/* What asc_calibration_sheet_apply answers. */
#define ASC_CALIBRATION_APPLY_OK 0              /* the changes were written */
#define ASC_CALIBRATION_APPLY_ENDED 1           /* the sheet ended: nothing to apply */
#define ASC_CALIBRATION_APPLY_NOTHING_CERTAIN 2 /* no row is certain */
#define ASC_CALIBRATION_APPLY_TOO_MANY 3        /* more changes than capacity: *count says how many */
#define ASC_CALIBRATION_APPLY_INVALID 4         /* a NULL argument */
#define ASC_CALIBRATION_APPLY_ALREADY_APPLIED 5 /* the sheet was applied once */

/* What asc_source_quit_decide answers. */
#define ASC_SOURCE_QUIT_KEEP 0             /* nothing changes */
#define ASC_SOURCE_QUIT_STOP 1             /* stop, post the "source" notice, drop a pending rebuild */
#define ASC_SOURCE_QUIT_SWITCH_TO_SYSTEM 2 /* go on with whole-system audio, store it, post the notice */

/* Whether streaming is on when an app quits (asc_source_quit_decide's streaming). */
#define ASC_STREAMING_OFF 0        /* no session runs */
#define ASC_STREAMING_RUNNING 1    /* a session runs */
#define ASC_STREAMING_REBUILDING 2 /* a session is being rebuilt (a capture format change): counts as on */

/* The port the web remote listens on unless its configuration names another (unassigned at IANA). */
#define ASC_REMOTE_DEFAULT_PORT 7655

/* Why asc_remote_start returned NULL. */
#define ASC_REMOTE_ERROR_NONE 0        /* it didn't */
#define ASC_REMOTE_ERROR_CONFIG 1      /* the configuration doesn't read, or the callback is NULL */
#define ASC_REMOTE_ERROR_PORT_IN_USE 2 /* another program listens on the port: offer another */
#define ASC_REMOTE_ERROR_LISTEN 3      /* no address could be listened on for another reason */
#define ASC_REMOTE_ERROR_RANDOM 4      /* no randomness for the pairing code */
#define ASC_REMOTE_ERROR_FAILED 5      /* a panic while starting */

/* ---- Version and volume ------------------------------------------------- */

/* The crate version, for example "0.1.0" (static string, never freed). */
const char *asc_version(void);

/* The interface version the library implements; compare with ASC_ABI_VERSION. */
uint32_t asc_abi_version(void);

/* Linear gain of a volume slider position 0...100: (p / 100)^3, clamped. */
double asc_volume_gain(double position);

/* Volume slider position of a linear gain 0...1: 100 * cbrt(g), clamped. */
double asc_volume_position(double gain);

/* A local output's linear gain: asc_volume_gain(local) * asc_volume_gain(master). */
double asc_output_gain(double local_position, double master_position);

/*
 * Multiplies frames x 2 interleaved samples by gain, in place. Gain 0 (or a
 * negative or non-finite gain) gives exact zeros; any NaN or infinity in the
 * result becomes 0. Real-time safe.
 */
void asc_apply_gain(float *interleaved, size_t frames, float gain);

/* ---- Equalizer ---------------------------------------------------------- */

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

/*
 * An equalizer. asc_eq_process runs on the capture thread; every other
 * asc_eq_* call is a control call.
 */
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

/* ---- Delay ring, writer and readers ------------------------------------- */

/*
 * The shared delay ring: interleaved stereo float32 frames at the capture
 * rate, written by one writer and read by any number of readers, each at its
 * own delay. Its query calls run on any thread.
 */
typedef struct AscRing AscRing;

/*
 * The ring's single writer. asc_ring_write runs on the capture thread;
 * asc_ring_writer_reset runs on the control thread while capture is stopped.
 */
typedef struct AscRingWriter AscRingWriter;

/*
 * A delay reader bound to one ring, one per local output. The read calls run
 * on that output's IO thread; every other asc_reader_* call runs on any thread.
 */
typedef struct AscReader AscReader;

/* What one read did. */
typedef struct AscReadReport {
    size_t frames;       /* frames written to dst */
    int64_t written;     /* frames written to the ring when the read started (W) */
    int64_t delay;       /* the effective delay applied, in frames */
    int64_t error;       /* how far behind (positive) or ahead (negative) the reader was */
    int64_t start;       /* ring index of the block's first frame */
    int64_t lag;         /* the published lag: W minus the next frame to read */
    int32_t correction;  /* an ASC_CORRECTION_* code */
    size_t masked;       /* leading frames of silence: already played, gone, before the stream,
                            or a whole block while the reader waits to anchor */
    size_t copied;       /* frames copied from the ring */
    size_t starved;      /* trailing frames of silence: the ring had nothing newer */
    bool rejected;       /* the writer overtook the copy, which became silence */
} AscReadReport;

/* A reader's diagnostic counters: monotonic, read without blocking the reader. */
typedef struct AscReaderCounters {
    uint64_t primes;                /* anchors because the reader was not anchored */
    uint64_t resyncs;               /* anchors that applied a requested resync */
    uint64_t drift_skips;           /* skip-forward corrections (behind, or overrun) */
    uint64_t drift_skip_frames;     /* frames those corrections skipped */
    uint64_t drift_silences;        /* step-back corrections (ahead) */
    uint64_t drift_silence_frames;  /* frames of silence those corrections inserted */
    uint64_t starved_renders;       /* reads that reached the newest frame before the block was full */
    uint64_t starved_frames;        /* frames of silence those reads ended with */
    uint64_t rejected_copies;       /* copies the writer overtook */
} AscReaderCounters;

/* Ring capacity in frames for 24 s at capture rate `rate`; 0 outside 8000...384000 Hz. */
size_t asc_ring_capacity_for_rate(uint32_t rate);

/*
 * Creates an empty ring of capacity_frames frames. Returns NULL outside
 * ASC_RING_MIN_FRAMES...ASC_RING_MAX_FRAMES.
 */
AscRing *asc_ring_create(size_t capacity_frames);

/* Destroys a ring handle; its writer and readers keep the frames alive. */
void asc_ring_destroy(AscRing *ring);

/* The capacity in frames. */
size_t asc_ring_capacity(const AscRing *ring);

/* Whether the ring holds 24 s at capture rate `rate`, which also lies in 8000...384000 Hz. */
bool asc_ring_supports_rate(const AscRing *ring, uint32_t rate);

/* Any thread: frames written since the last reset (the session's frames captured). */
uint64_t asc_ring_frames_written(const AscRing *ring);

/* Takes the ring's only writer; NULL while another writer exists. */
AscRingWriter *asc_ring_writer_create(AscRing *ring);

/* Destroys the writer, so the ring can hand out a new one. */
void asc_ring_writer_destroy(AscRingWriter *writer);

/*
 * Capture thread: appends `frames` stereo frames as one block, stamped with
 * the time of the call on the core's monotonic clock, so a converting output
 * can follow the capture's clock (S207). Any count is accepted; once the ring
 * is full the oldest frames are overwritten, and a write larger than the ring
 * keeps only its last frames. Never waits for readers. Real-time safe.
 */
void asc_ring_write(AscRingWriter *writer, const float *interleaved, size_t frames);

/*
 * Control thread, at a session start: makes all history unreadable and
 * restarts the frame count at 0. Call it only while no capture callback runs
 * and no reader of the ring exists.
 */
void asc_ring_writer_reset(AscRingWriter *writer);

/*
 * Creates a reader of `ring` for a session captured at capture_rate (Hz). Its
 * delay starts at 0 (one block) and its drift tolerance at 50 ms; it anchors
 * on its first read that finds more than its delay written, and reads silence
 * until then. Returns NULL outside 8000...384000 Hz.
 */
AscReader *asc_reader_create(AscRing *ring, uint32_t capture_rate);

/* Destroys a reader. */
void asc_reader_destroy(AscReader *reader);

/*
 * Output thread: fills dst with exactly `frames` stereo frames, read at the
 * delay set by asc_reader_set_delay_frames. Steady state plays every frame
 * once, in order; a delay change is exact; drift is corrected outside the
 * tolerance; silence fills what the ring can't give, and no frame plays twice
 * (until a prime reset). A reader that has played everything a stopped writer
 * wrote makes no correction, and a delay change waits for the writer's next
 * frames. A read of 0 frames does nothing: a pending resync or prime reset
 * waits for the next read, and the report is all zeros. report may be NULL.
 * Real-time safe.
 */
void asc_reader_read(AscReader *reader, float *dst, size_t frames, AscReadReport *report);

/*
 * Output thread: as asc_reader_read, at delay_frames with tolerance_frames
 * given by the caller instead of the stored ones. Real-time safe.
 */
void asc_reader_read_at(AscReader *reader, float *dst, size_t frames, int64_t delay_frames,
                        int64_t tolerance_frames, AscReadReport *report);

/* Any thread: sets the target delay in frames (negative counts as 0) and requests a resync. */
void asc_reader_set_delay_frames(AscReader *reader, int64_t frames);

/* Any thread: the target delay in frames. */
int64_t asc_reader_delay_frames(const AscReader *reader);

/* Any thread: sets the drift tolerance in frames (negative counts as 0). */
void asc_reader_set_tolerance_frames(AscReader *reader, int64_t frames);

/* Any thread: the drift tolerance in frames. */
int64_t asc_reader_tolerance_frames(const AscReader *reader);

/* Any thread: asks the reader to re-anchor at the target delay, on its next read with unread frames. */
void asc_reader_request_resync(AscReader *reader);

/*
 * Any thread: asks the next read to start over like a new reader (it anchors
 * once more than its delay is written, and counts a prime); the counters are
 * kept.
 */
void asc_reader_reset_prime(AscReader *reader);

/* Any thread: the lag published by the last read, in frames (delay minus one block when steady). */
int64_t asc_reader_lag_frames(const AscReader *reader);

/* Any thread: a snapshot of the counters; fields may come from different reads. */
AscReaderCounters asc_reader_counters(const AscReader *reader);

/* ---- Rate conversion and the output stage ------------------------------- */

/*
 * A streaming stereo resampler that carries its phase across blocks, e.g.
 * from the capture rate to an output device's rate. All calls on one
 * resampler run on one thread at a time.
 */
typedef struct AscResampler AscResampler;

/* What one asc_resampler_process call consumed and produced. */
typedef struct AscResampleCount {
    size_t input_frames;   /* input frames consumed */
    size_t output_frames;  /* output frames produced */
} AscResampleCount;

/*
 * The output stage of one local output: channel mapping and gain. The gain
 * calls run on any thread; the apply and render calls run on the output thread.
 */
typedef struct AscGainStage AscGainStage;

/*
 * Creates a resampler; NULL unless both rates lie in 8000...384000 Hz. Equal
 * rates pass samples through unchanged.
 */
AscResampler *asc_resampler_create(uint32_t input_rate, uint32_t output_rate);

/* Destroys a resampler. */
void asc_resampler_destroy(AscResampler *resampler);

/* Changes the input rate and starts a new stream; false for an unsupported rate. Never allocates. */
bool asc_resampler_set_input_rate(AscResampler *resampler, uint32_t input_rate);

/* Starts a new stream: clears the history and the phase. Real-time safe. */
void asc_resampler_reset(AscResampler *resampler);

/* The content delay in input frames (0 when passing through). Real-time safe. */
size_t asc_resampler_latency_frames(const AscResampler *resampler);

/*
 * Input frames still needed before the next output_frames output frames can
 * be produced, so the output thread can size each reader request from the
 * converter's needs; saturates at SIZE_MAX. Real-time safe.
 */
size_t asc_resampler_input_frames_for(const AscResampler *resampler, size_t output_frames);

/*
 * Converts input_frames stereo frames into at most output_frames stereo
 * frames. Consumes input until it runs out or the output is full; call again
 * with the rest. When the rates differ, non-finite input becomes silence and
 * clears the history. Equal rates pass every sample through unchanged, NaN
 * and infinity included; the output stage deals with those. Real-time safe.
 */
AscResampleCount asc_resampler_process(AscResampler *resampler, const float *input, size_t input_frames,
                                       float *output, size_t output_frames);

/* Creates an output stage at linear gain `gain` (negative or non-finite counts as 0). */
AscGainStage *asc_gain_stage_create(float gain);

/* Destroys an output stage. */
void asc_gain_stage_destroy(AscGainStage *stage);

/*
 * Any thread: sets the linear gain (negative or non-finite counts as 0). The
 * output thread ramps to it across its next buffer and holds it after that.
 */
void asc_gain_stage_set_gain(AscGainStage *stage, float gain);

/* Any thread: the gain last set. */
float asc_gain_stage_gain(const AscGainStage *stage);

/*
 * Output thread: multiplies frames x channels interleaved samples by the gain
 * in place. Gain 0 gives exact zeros and any NaN or infinity becomes 0; the
 * first buffer plays at the gain already set. Real-time safe.
 */
void asc_gain_stage_apply(AscGainStage *stage, float *interleaved, size_t frames, size_t channels);

/*
 * Output thread: maps `frames` stereo frames onto a device buffer of
 * frames x channels interleaved samples (L to the first channel, R to the
 * second, 0 to the rest; (L + R) / 2 on a one-channel device), then applies
 * the gain as asc_gain_stage_apply does. Returns the frames written; 0 when
 * channels is 0. Real-time safe.
 */
size_t asc_gain_stage_render(AscGainStage *stage, const float *stereo, size_t frames, float *device,
                             size_t channels);

/* ---- Renderer: a local output with its fades (S62-S65, S182-S188) ------- */

/*
 * One local output's renderer for one start on one device: its own delay
 * reader on the ring, the converter to the device rate, the channel mapping,
 * the gain and the 20 ms fades. It starts closed and fades in on its first
 * audio; a stop's fade-out and every delay change fade (S184 to S188). The
 * render calls run on the output thread; every other call runs on any thread
 * and is a lock-free atomic operation.
 */
typedef struct AscRenderer AscRenderer;

/* The longest a host waits for its outputs' fade-outs at a stop or a removal (S186). */
#define ASC_STOP_FADE_CAP_MS 500

/* asc_renderer_fade_state: exact zeros, fading in, full level, fading out. */
#define ASC_FADE_CLOSED 0
#define ASC_FADE_FADING_IN 1
#define ASC_FADE_OPEN 2
#define ASC_FADE_FADING_OUT 3

/*
 * Control thread: a renderer reading `ring`, captured at capture_rate, for a
 * device running at device_rate with `channels` interleaved channels, at
 * linear gain `gain`; its delay is 0 until one is set. NULL for a NULL ring,
 * no channels, or a rate outside 8000...384000 Hz.
 */
AscRenderer *asc_renderer_create(AscRing *ring, uint32_t capture_rate, uint32_t device_rate, size_t channels,
                                 float gain);

/* Destroys a renderer, once the output thread no longer calls it. */
void asc_renderer_destroy(AscRenderer *renderer);

/*
 * Output thread: fills `frames` frames of frames x channels interleaved
 * floats: read at the output's delay, converted to the device rate, mapped
 * onto the channels, at the gain, faded. Real-time safe.
 */
void asc_renderer_render(AscRenderer *renderer, float *device, size_t frames);

/*
 * Output thread: as asc_renderer_render, for a callback whose first frame is
 * output at output_time_ns host nanoseconds, so that a stop's fade-out
 * publishes when its last frame is output. The callback reads at the time of
 * the call on the core's clock, the one asc_ring_write stamps the capture's
 * blocks with, so a converting output follows the capture's clock (S207).
 * Real-time safe.
 */
void asc_renderer_render_timed(AscRenderer *renderer, float *device, size_t frames, uint64_t output_time_ns);

/* Any thread: the linear gain (negative or non-finite counts as 0), ramped to across the next callback. */
void asc_renderer_set_gain(AscRenderer *renderer, float gain);

/* Any thread: the gain last set. */
float asc_renderer_gain(const AscRenderer *renderer);

/*
 * Any thread: a new delay in capture-rate frames. A renderer that plays
 * fades out first and hands it to its reader after the fade-out's last
 * frame, then fades in on the new timeline; a closed one takes it at once.
 */
void asc_renderer_set_delay_frames(AscRenderer *renderer, int64_t frames);

/* Any thread: the newest delay asked for, in capture-rate frames. */
int64_t asc_renderer_delay_frames(const AscRenderer *renderer);

/* Control thread, before the renderer's first callback: whether its device is
   Bluetooth, which adds a capture block to its start lead (S230). */
void asc_renderer_set_bluetooth(AscRenderer *renderer, bool bluetooth);

/* Any thread: what the start lead adds for a Bluetooth device, in capture
   frames (S230); 0 for any other device or a NULL handle. */
int64_t asc_renderer_extra_lead_frames(const AscRenderer *renderer);

/*
 * Control thread, after asc_renderer_set_bluetooth: the output latency the
 * device reported, in ns; returns the one to use (S231). A Bluetooth
 * device's report outside 50 to 1000 ms gives way to last_plausible_ns (the
 * latest report accepted for the same output, 0 for none), else 150 ms. A
 * NULL handle returns reported_ns.
 */
uint64_t asc_renderer_set_reported_latency(AscRenderer *renderer, uint64_t reported_ns,
                                           uint64_t last_plausible_ns);

/*
 * Any thread: the output latency the device reported, in ns, when the
 * window rejected it (S231); false, leaving *latency_ns alone, otherwise.
 */
bool asc_renderer_rejected_latency_ns(const AscRenderer *renderer, uint64_t *latency_ns);

/*
 * Any thread, before the output stops: fade out over 20 ms of device frames
 * from the next callback, then render exact zeros until the output stops.
 */
void asc_renderer_request_fade_out(AscRenderer *renderer);

/* Any thread: whether the requested fade-out's last frame has been rendered. */
bool asc_renderer_fade_rendered(const AscRenderer *renderer);

/*
 * Any thread: the host time in nanoseconds at which the requested fade-out's
 * last frame is output; false, leaving *time_ns alone, until a timed render
 * has rendered it.
 */
bool asc_renderer_fade_end_time_ns(const AscRenderer *renderer, uint64_t *time_ns);

/*
 * Any thread: whether the requested fade-out has played by host time now_ns,
 * that is its last frame's output time plus latency_ns (the device's and its
 * stream's latency) has passed.
 */
bool asc_renderer_faded_by_time(const AscRenderer *renderer, uint64_t latency_ns, uint64_t now_ns);

/* Any thread: where the fade was at the end of the latest callback, an ASC_FADE_* code. */
int32_t asc_renderer_fade_state(const AscRenderer *renderer);

/* Any thread: the frames of a fade at the device rate, 20 ms (960 at 48 kHz). */
size_t asc_renderer_fade_frames(const AscRenderer *renderer);

/* Any thread: the reader's lag, W - P after its latest read (S56). */
int64_t asc_renderer_lag_frames(const AscRenderer *renderer);

/* Any thread: the reader's counters; all zero for a NULL handle. */
AscReaderCounters asc_renderer_counters(const AscRenderer *renderer);

/* Any thread: the capture frames the renderer slowed its converter by to keep
   its reader off the newest frame (S206) or to follow the capture's clock
   (S207), in all; 0 for a NULL handle. */
uint64_t asc_renderer_slowed_frames(const AscRenderer *renderer);

/* Any thread: the capture frames the renderer sped its converter up by to
   follow the capture's clock (S207), in all; 0 for a NULL handle. */
uint64_t asc_renderer_sped_frames(const AscRenderer *renderer);

/* Any thread: the largest lateness of a callback against the renderer's
   earliest ones, in ns (S209); 0 without output times or for a NULL handle. */
uint64_t asc_renderer_late_max_ns(const AscRenderer *renderer);

/* Any thread: the callbacks that came more than 5 ms later than
   the renderer's earliest ones (S209); 0 for a NULL handle. */
uint64_t asc_renderer_late_callbacks(const AscRenderer *renderer);

/* Any thread: the device frames the converter couldn't produce and that were
   played as silence, in all (0 unless the read sizing is wrong); 0 for a NULL
   handle. */
uint64_t asc_renderer_shortfall_frames(const AscRenderer *renderer);

/* Any thread: the capture frames by which the renderer's slow-down made up
   shortages of its reader (S206), apart from the trims that follow the
   capture's clock, in all; 0 for a NULL handle. */
uint64_t asc_renderer_shortage_frames(const AscRenderer *renderer);

/* Any thread: the observer's least slack, in ns of capture frames: W - P at
   the start of a callback less what the callback needs, below 0 when a
   callback found too few frames (S229). False, leaving *slack_ns alone, until
   the output has played its warm-up and a callback was measured since, and
   for a NULL handle. */
bool asc_renderer_least_slack_ns(const AscRenderer *renderer, int64_t *slack_ns);

/* Any thread: the observer's largest drop of the lateness floor below the
   anchor floor set at the reader's latest anchor, in ns (S229); 0 without
   output times or for a NULL handle. */
uint64_t asc_renderer_floor_drop_ns(const AscRenderer *renderer);

/* Any thread: the observer's steps in the device's output times, callbacks
   whose output time lay more than 1 ms off the previous callback's plus its
   frames (S229); 0 without output times or for a NULL handle. */
uint64_t asc_renderer_time_steps(const AscRenderer *renderer);

/* ---- Delay helpers ------------------------------------------------------- */

/*
 * A delay input from the user, ready to store: clamped to 0...20 000 ms into
 * *sanitized_ms. False, writing nothing, for a non-finite value (which is
 * ignored) or a NULL sanitized_ms.
 */
bool asc_delay_sanitize_ms(double delay_ms, double *sanitized_ms);

/*
 * A delay in frames at `rate` Hz: floor(delay_ms x rate / 1000) into *frames
 * (a negative delay counts as 0). False, writing nothing, for a non-finite
 * delay or a NULL frames.
 */
bool asc_delay_ms_to_frames(double delay_ms, uint32_t rate, int64_t *frames);

/* ---- Local delay model (offsets relative to a reference output) ---------- */

/*
 * The local delay model replaces the old AirPlay sync rule (S78): one local
 * output, the reference (the built-in speakers on the Mac), defines time 0
 * and has no delay setting; every other local output has an offset relative
 * to it, -2000 to +20 000 ms, that always applies. The host passes the
 * outputs taking part (switched on, with their devices listed) and gets each
 * one's hold-back, the delay its renderer plays at. While an AirPlay speaker
 * is chosen, the AirPlay latency L comes on top: the bundled engine's stored
 * AirPlay delay, or the native sender's reported latency plus the user's
 * adjustment (asc_local_native_latency_ms).
 *
 * Re-timing: whenever the outputs taking part, an offset, the reference, the
 * AirPlay condition or L change, the host computes the hold-backs again and
 * gives each running renderer whose hold-back changed the new one through
 * asc_reader_set_delay_frames (more inserts silence, less skips forward);
 * a renderer whose hold-back is unchanged gets no call. An output being
 * attached starts at its hold-back.
 */

/* A local output taking part. */
typedef struct AscLocalOutput {
    double offset_ms;   /* its stored offset; ignored for the reference */
    bool is_reference;  /* the reference: its offset counts as 0 */
} AscLocalOutput;

/*
 * The hold-back in ms of each of the `count` outputs taking part, in the same
 * order, into hold_backs_ms: its offset (0 for the reference) minus the
 * smallest offset taking part, or minus -L when that is smaller and
 * airplay_chosen, capped at 20 000 ms; never negative. An offset out of range
 * counts clamped, one that isn't finite as 0; airplay_latency_ms counts only
 * when airplay_chosen, clamped to 0...20 000 (not finite: 0). False, writing
 * nothing, for a NULL array with a nonzero count.
 */
bool asc_local_hold_backs_ms(const AscLocalOutput *outputs, size_t count, bool airplay_chosen, double airplay_latency_ms, double *hold_backs_ms);

/*
 * An offset input from the user, ready to store: clamped to -2000...20 000 ms
 * into *sanitized_ms. False, writing nothing, for a non-finite value (which
 * is ignored) or a NULL sanitized_ms.
 */
bool asc_local_offset_sanitize_ms(double offset_ms, double *sanitized_ms);

/*
 * The bundled engine's AirPlay delay input, ready to store: clamped to
 * 0...20 000 ms into *sanitized_ms (2000 by default). False, writing nothing,
 * for a non-finite value (which is ignored) or a NULL sanitized_ms.
 */
bool asc_local_airplay_delay_sanitize_ms(double delay_ms, double *sanitized_ms);

/*
 * L under the native sender: its reported base latency plus the user's
 * adjustment (-2000...20 000 ms, 0 by default; out of range counts clamped,
 * not finite as 0), clamped to 0...20 000 ms. The window's AirPlay delay
 * field shows this value.
 */
double asc_local_native_latency_ms(double sender_latency_ms, double adjustment_ms);

/*
 * The adjustment to store when the user enters shown_ms in the AirPlay delay
 * field under the native sender: shown_ms minus the sender's latency of that
 * moment, clamped to -2000...20 000 ms, into *adjustment_ms. False, writing
 * nothing, for a non-finite value or a NULL adjustment_ms.
 */
bool asc_local_native_adjustment_ms(double shown_ms, double sender_latency_ms, double *adjustment_ms);

/*
 * One device UID for the one-time migration of the stored absolute delays:
 * every UID with a stored delay, and every one without that is switched on
 * or in the device list of that moment.
 */
typedef struct AscStoredDelay {
    bool is_reference;       /* the reference (the first one marked counts) */
    bool has_stored_delay;   /* it has a stored absolute delay */
    double stored_delay_ms;  /* that delay; read clamped to 0...20 000, not finite counts as none */
} AscStoredDelay;

/* The offset the migration stores for one UID. */
typedef struct AscMigratedOffset {
    bool store;        /* store offset_ms; false for the reference, which gets none */
    double offset_ms;  /* -2000...20 000 */
} AscMigratedOffset;

/* The AirPlay settings the migration stores. */
typedef struct AscDelayMigration {
    double airplay_delay_ms;      /* the bundled engine's AirPlay delay */
    double native_adjustment_ms;  /* the native sender's adjustment: 0 */
} AscDelayMigration;

/*
 * The one-time migration of the stored absolute delays: one offset per
 * entry into `offsets`, in the same order, and the AirPlay settings into
 * *result. D0, what an output without a stored delay played at, is the
 * legacy single delay (when has_legacy_delay) clamped to 0...20 000, else
 * 2000; D_R is the reference's stored delay, else D0. The AirPlay delay
 * becomes D_R when it is at least 1000 ms, else 2000; another UID with a
 * stored delay D gets D - D_R; one without gets D0 - (the new AirPlay delay)
 * when D0 is at least 1000 ms, else D0 - D_R; each clamped to
 * -2000...20 000. The host stores the result under new keys, once, and
 * leaves the old ones in place. False, writing nothing, for a NULL array
 * with a nonzero count or a NULL result.
 */
bool asc_local_delay_migrate(const AscStoredDelay *entries, size_t count, bool has_legacy_delay, double legacy_delay_ms, AscMigratedOffset *offsets, AscDelayMigration *result);

/* ---- Output device volume (the operating system's own, per device) ------- */

/*
 * The operating system keeps a volume and a mute for every output device and
 * applies them after AudioSync's gain, so a device left at 13 % sounds very
 * quiet whatever AudioSync's slider says. The host reads both for each local
 * output that is switched on (on the Mac through Core Audio, only the parts
 * that are present and settable), asks here what the reading means and what
 * the one-click fix writes, and writes only when the user clicks that fix.
 * Pure functions for the control thread.
 */

/* One reading. A part the host couldn't read, or can't set, has its has_ flag false. */
typedef struct AscDeviceVolume {
    bool has_volume;  /* the volume is known */
    double volume;    /* the volume, 0 to 1 (as the system's slider for the device shows it) */
    bool has_mute;    /* the mute is known */
    bool muted;       /* the device is muted */
} AscDeviceVolume;

/* What a reading means for the row, and what one click on its fix writes. */
typedef struct AscDeviceVolumeDecision {
    uint32_t decision;     /* ASC_DEVICE_VOLUME_... */
    bool shows_hint;       /* LOW, MUTED or MUTED_AND_LOW: the row shows a hint and the fix */
    bool has_percent;      /* the volume is known and finite */
    uint32_t percent;      /* round(100 x clamp(volume, 0, 1)), half away from zero */
    bool fix_sets_volume;  /* the fix sets the volume to fix_volume: only when LOW or MUTED_AND_LOW */
    double fix_volume;     /* 0.75; never lower than a volume that isn't low */
    bool fix_unmutes;      /* the fix clears the mute: only when muted */
} AscDeviceVolumeDecision;

/*
 * What `reading` means: MUTED when the mute is known and set, LOW when the
 * volume is known and 25 % or less, MUTED_AND_LOW when both hold, UNKNOWN when
 * neither part is known, FINE otherwise; a volume that isn't finite counts as
 * unknown. The fix writes each part at most once.
 */
AscDeviceVolumeDecision asc_device_volume_decide(AscDeviceVolume reading);

/*
 * Whether a new reading whose decision is `now` gets a debug log line after
 * one whose decision was `previous`: only when they differ. The first reading
 * (has_previous false) logs a line of its own, so it is no change here; a
 * value that is no ASC_DEVICE_VOLUME_ decision gives false.
 */
bool asc_device_volume_decision_changed(bool has_previous, uint32_t previous, uint32_t now);

/*
 * A decision's name in the debug log ("unknown", "fine", "low", "muted",
 * "muted and low") as a static string; NULL for a value that is no decision.
 */
const char *asc_device_volume_decision_name(uint32_t decision_code);

/* ---- When the source app quits ------------------------------------------ */

/*
 * What a session does when an app quits (S227): ASC_SOURCE_QUIT_STOP or
 * ASC_SOURCE_QUIT_SWITCH_TO_SYSTEM only when streaming is on and `quit_app`
 * is the bundle ID of the app `source` captures, else ASC_SOURCE_QUIT_KEEP.
 * `setting` is the stored setting ("system" switches; anything else, NULL
 * included, stops) and `source` the stored source ("" or NULL the whole
 * system, "input:<uid>" an input device, else an app's bundle ID). A string
 * that isn't UTF-8 counts as NULL and an unknown `streaming` as
 * ASC_STREAMING_OFF. Pure function for the control thread.
 */
uint32_t asc_source_quit_decide(const char *setting, const char *source, const char *quit_app, uint32_t streaming);

/* ---- Microphone calibration ---------------------------------------------- */

/*
 * One local output measured against the reference with a microphone
 * (S232-S234): the host plays the test signal on the reference and records
 * it, then on the chosen output with the microphone where it was, and
 * passes both recordings, mono at the signal's rate. Frame 0 of each is the
 * moment the signal enters that output's path, before its hold-back, with
 * the same gap from recording start to play start in both runs (a gap that
 * differs goes into the result unseen). Each recording lasts the largest
 * hold-back taking part (about 2 s with AirPlay, up to 20 s with large
 * offsets) plus the output's latency plus the 0.75 s signal plus about
 * 0.5 s. Record at 48 kHz: the 2^21-frame limit is about 44 s there but
 * only 10.9 s at 192 kHz. Both calls allocate and are for the control
 * thread; the estimate takes tens of ms and up to about 96 MB.
 */

/* Frames of the test signal at `rate`; 0 for a rate outside 22 050...384 000. */
size_t asc_calibration_signal_frames(uint32_t rate);

/*
 * The test signal at `rate`, mono: a 0.75 s linear sweep from 300 Hz to
 * 8 kHz peaking at -18 dBFS, with 20 ms raised-cosine fades. False, writing
 * nothing, for an unsupported rate, a `frames` other than
 * asc_calibration_signal_frames(rate) or a NULL samples.
 */
bool asc_calibration_signal(uint32_t rate, float *samples, size_t frames);

/* What asc_calibration_estimate found. */
typedef struct AscCalibration {
    uint32_t status;      /* an ASC_CALIBRATION_ code */
    uint32_t recording;   /* an ASC_CALIBRATION_RECORDING_ code */
    double offset_ms;     /* the new offset, whole ms; refused: the old one as stored offsets count */
    double difference_ms; /* how much later the chosen output was heard; refused: 0 */
    double reference_ms;  /* where the signal arrived in the reference's recording; refused: 0 */
    double chosen_ms;     /* where it arrived in the chosen output's recording; refused: 0 */
    bool clamped;         /* accepted, but offset_ms hit an end of the range: the output stays off by the rest */
} AscCalibration;

/*
 * The chosen output's new offset into *result: its old offset (0 when not
 * finite, else clamped to -2000...+20 000) minus how much later it was
 * heard than the reference, rounded to whole ms and clamped to that range;
 * or a refusal that changes nothing. False, writing nothing, for a NULL
 * array with a nonzero count or a NULL result.
 */
bool asc_calibration_estimate(const float *signal, size_t signal_frames, const float *reference, size_t reference_frames, const float *chosen, size_t chosen_frames, uint32_t rate, double old_offset_ms, AscCalibration *result);

/*
 * Calibrate All Speakers: the reference's sweep, then every other speaker's
 * two, one speaker at a time, each estimated against the reference as
 * asc_calibration_estimate does. A row's result is the mean of its two
 * sweeps with an uncertainty of half their difference plus 3 ms; over
 * 10 ms the row is uncertain, and any refused sweep refuses the row. The
 * AirPlay speakers share one delay, the median of the certain AirPlay rows.
 * Nothing changes until the host writes what asc_calibration_sheet_apply
 * lists. Speakers are named by their index in the array given to
 * asc_calibration_run_create; rows keep that order. Every call is for one
 * control thread at a time per handle; recording runs an FFT.
 */
typedef struct AscCalibrationRun AscCalibrationRun;
typedef struct AscCalibrationSheet AscCalibrationSheet;

/* A speaker that is on when the run starts. */
typedef struct AscCalibrationSpeaker {
    uint32_t kind;     /* an ASC_CALIBRATION_KIND_ code */
    bool has_offset;   /* false: no stored delay (a Cast speaker's first window is then the longest) */
    double offset_ms;  /* its stored delay; every AirPlay speaker carries the shared AirPlay delay */
    double latency_ms; /* when its sweep is expected, from the moment it enters its path before the hold-back: hold-back plus latency, the reference's too */
} AscCalibrationSpeaker;

/* What to play and record next. */
typedef struct AscCalibrationStep {
    size_t row;           /* the speaker's index */
    uint32_t sweep;       /* 0 the reference's sweep, else the speaker's first or second */
    size_t window_frames; /* frames to record from the moment the sweep enters the speaker's path */
    bool searching;       /* a Cast speaker without a stored offset: the longest window, show that it searches */
} AscCalibrationStep;

/* One row of the sheet. */
typedef struct AscCalibrationRow {
    uint32_t state;        /* an ASC_CALIBRATION_ROW_ code */
    uint32_t reason;       /* refused: an ASC_CALIBRATION_ refusal code; else 0 */
    uint32_t recording;    /* refused: an ASC_CALIBRATION_RECORDING_ code; else 0 */
    bool has_old;          /* the speaker had a stored delay */
    double old_ms;         /* that delay as given */
    double new_ms;         /* certain or uncertain: the new delay, whole ms; else 0 */
    double uncertainty_ms; /* certain or uncertain: the U of new_ms +- U; else 0 */
    double difference_ms;  /* certain or uncertain: how much later than the reference it was heard */
    bool clamped;          /* new_ms hit an end of its range */
} AscCalibrationRow;

/* The shared AirPlay delay. */
typedef struct AscCalibrationAirPlay {
    uint32_t state;      /* an ASC_CALIBRATION_AIRPLAY_ code */
    bool has_old;        /* the AirPlay speakers carried a delay */
    double old_ms;       /* that delay as given */
    double new_ms;       /* the median, whole ms; NONE_CERTAIN: old_ms */
    double spread_ms;    /* the largest minus the smallest difference among the certain rows */
    size_t certain_rows; /* how many AirPlay rows are certain */
} AscCalibrationAirPlay;

/* One change Apply lists; writing back old_ms, or removing the stored value when has_old is false, undoes it. */
typedef struct AscCalibrationChange {
    size_t row;    /* the speaker's index; 0 for the AirPlay delay */
    bool airplay;  /* the one AirPlay delay rather than the speaker on row */
    bool has_old;  /* the value replaced was stored */
    double old_ms; /* the value replaced */
    double new_ms; /* the value to write */
} AscCalibrationChange;

/*
 * A run over `count` speakers, `reference` the index of the reference, a
 * local output. NULL for an unsupported rate, a reference that isn't a
 * local output among them, only the reference, an unknown kind or a NULL
 * array with a nonzero count.
 */
AscCalibrationRun *asc_calibration_run_create(uint32_t rate, const AscCalibrationSpeaker *speakers, size_t count, size_t reference);

/* The next step into *step; false, writing nothing, once the run is over or for a NULL argument. */
bool asc_calibration_run_step(const AscCalibrationRun *run, AscCalibrationStep *step);

/*
 * The recording the last step asked for, mono at the run's rate. A refused
 * reference sweep refuses every row and ends the steps; a refused first
 * sweep skips the second. False when no step waits or for a NULL argument.
 */
bool asc_calibration_run_record(AscCalibrationRun *run, const float *recording, size_t frames);

/* Ends the run with an ASC_CALIBRATION_END_ reason (unknown counts as cancelled); its sheet applies nothing. */
void asc_calibration_run_cancel(AscCalibrationRun *run, uint32_t reason);

/* The run's sheet; frees the run. A run with steps still waiting ends as cancelled. NULL for a NULL run. */
AscCalibrationSheet *asc_calibration_run_finish(AscCalibrationRun *run);

/* Frees a run without a sheet. */
void asc_calibration_run_destroy(AscCalibrationRun *run);

/* The number of rows, one per speaker; 0 for a NULL sheet. */
size_t asc_calibration_sheet_rows(const AscCalibrationSheet *sheet);

/* Row `index` into *row; false, writing nothing, out of range or for a NULL argument. */
bool asc_calibration_sheet_row(const AscCalibrationSheet *sheet, size_t index, AscCalibrationRow *row);

/* The shared AirPlay delay into *airplay; false, writing nothing, without AirPlay speakers or for a NULL argument. */
bool asc_calibration_sheet_airplay(const AscCalibrationSheet *sheet, AscCalibrationAirPlay *airplay);

/* Why the sheet ended, an ASC_CALIBRATION_END_ code; END_NONE while it can be applied. */
uint32_t asc_calibration_sheet_ended(const AscCalibrationSheet *sheet);

/*
 * The changes Apply writes, one per certain local or Cast row and one for
 * the AirPlay delay when an AirPlay row is certain, into `changes` and
 * their number into *count, with an ASC_CALIBRATION_APPLY_ code. Only
 * APPLY_OK writes changes, and spends the sheet: from then on its rows
 * describe offsets no longer stored, so a second Apply answers
 * APPLY_ALREADY_APPLIED and Measure Again is refused. APPLY_TOO_MANY sets
 * *count to the number needed, never more than the rows, and leaves the
 * sheet as it was. Keep the old values of one Apply for its Undo.
 */
uint32_t asc_calibration_sheet_apply(AscCalibrationSheet *sheet, AscCalibrationChange *changes, size_t capacity, size_t *count);

/*
 * Measure Again: a run of a new reference sweep and row `index`'s two,
 * from the values the sheet started with; its steps name the sheet's rows.
 * NULL for the reference's row, a row out of range, an ended, applied or
 * NULL sheet. Finish it and pass its sheet to asc_calibration_sheet_replace.
 */
AscCalibrationRun *asc_calibration_sheet_measure_again(const AscCalibrationSheet *sheet, size_t index);

/*
 * Puts the row of a Measure Again sheet in place of the old one and frees
 * `again`. False, leaving `sheet` and its rows as they were, for an `again`
 * that was cancelled or ended by a change, one not from this sheet's
 * Measure Again, or a NULL argument; `again` is freed either way.
 */
bool asc_calibration_sheet_replace(AscCalibrationSheet *sheet, AscCalibrationSheet *again);

/* Frees a sheet. */
void asc_calibration_sheet_destroy(AscCalibrationSheet *sheet);

/* ---- AirPlay sender (AudioSync's own, airplay/) ----------------------------- */

/*
 * AudioSync's own AirPlay sender: discovery, speakers added by the IDs
 * discovery reports, their volume and the master, their events, and the
 * capture's audio through an input handle. Nothing secret crosses this
 * interface outward: PINs and passwords go in only, credentials never leave
 * the core (it keeps them in the pairing store, asc_airplay_open_pairings).
 * Every asc_airplay_* call on a sender handle runs on one control thread at
 * a time, never an audio thread; the sender runs its own threads and
 * sockets. An input handle (AscAirPlayInput) has its own rules, below.
 *
 * A bug in the sender never ends the host: a panic inside a call returns
 * that call's failure value (NULL, false or 0) and makes its handle dead,
 * as does a panic that ends the sender's own driver thread or discovery's
 * mDNS daemon. A dead handle's calls do nothing and return their failure
 * values, except that
 * asc_airplay_next_event reports ASC_AIRPLAY_EVENT_FAILED once; the host
 * then destroys it (which still frees it) and may create a new sender. A
 * panic in an input write makes the input and its sender dead. The failure's
 * detail is where the panic happened, "panic at <file>:<line>" (the file
 * from its crate's folder on), or "latest panic at <file>:<line>" when one
 * of the sender's own threads ended and no call caught the panic, never
 * what it said, or "" when no panic is known; the first asc_airplay_create
 * installs a panic hook that notes the locations and then runs the hook that
 * was there before.
 */
typedef struct AscAirPlay AscAirPlay;

/*
 * The capture's way into a sender (asc_airplay_take_input). Its write runs
 * on one capture thread at a time, its rate request on any thread, and its
 * destroy once no write can run. It may outlive its sender, whose audio
 * then goes nowhere.
 */
typedef struct AscAirPlayInput AscAirPlayInput;

/*
 * One change discovery saw. The strings are NUL-terminated UTF-8 that the
 * handle owns, valid until the next asc_airplay_next_receiver call on it or
 * its destroy. A gone receiver has only its id; its name and model are "",
 * its availability ASC_AIRPLAY_AVAILABLE, its reason and flags 0.
 */
typedef struct AscAirPlayReceiver {
    int32_t event;        /* ASC_AIRPLAY_APPEARED, _CHANGED or _GONE */
    const char *id;       /* its 12-hex-digit device ID, or "raop:<instance>" / "airplay:<instance>" */
    const char *name;     /* the name it announces */
    const char *model;    /* its model, "" when it announces none */
    int32_t availability; /* ASC_AIRPLAY_AVAILABLE ... ASC_AIRPLAY_UNSUPPORTED */
    int32_t unsupported;  /* ASC_AIRPLAY_REASON_*: why it can't play */
    uint32_t flags;       /* ASC_AIRPLAY_FLAG_* bits */
    const char *target;   /* the "pair:<tsid>" or "theater:<group>" ID of the target it is a member of, or "" */
} AscAirPlayReceiver;

/*
 * One thing that happened to a speaker, or to the stream. The strings are
 * NUL-terminated UTF-8 that the handle owns, valid until the next
 * asc_airplay_next_event call on it or its destroy. Fields a kind doesn't
 * use are 0, except retry_ms and receiver_latency_frames, which are then -1,
 * and the strings, which are then "".
 */
typedef struct AscAirPlayEvent {
    int32_t kind;                    /* ASC_AIRPLAY_EVENT_* */
    const char *id;                  /* the speaker's receiver id; "" for a timeline event */
    int32_t state;                   /* a state event's ASC_AIRPLAY_STATE_* */
    int32_t error;                   /* an error event's ASC_AIRPLAY_ERROR_* */
    int32_t unsupported;             /* with ASC_AIRPLAY_ERROR_UNSUPPORTED, its ASC_AIRPLAY_REASON_* */
    int64_t retry_ms;                /* ms until the next session; -1 when the user must act */
    double latency_ms;               /* a latency event's L + d_s, a timeline event's L */
    int64_t receiver_latency_frames; /* a latency event's Audio-Latency; -1 when it sent none */
    bool restarted;                  /* a timeline event after a restart */
    const char *detail;              /* an error event's error, a failure event's panic location; for a debug log (no secrets) */
} AscAirPlayEvent;

/*
 * Starts an AirPlay sender: its stream, silent until audio comes through its
 * input (asc_airplay_take_input), and its driver thread; discovery waits for
 * asc_airplay_start_discovery. The master starts at 100. Returns NULL when
 * the thread can't start or creating the sender failed.
 */
AscAirPlay *asc_airplay_create(void);

/*
 * asc_airplay_create with the app's AirPlay 2 setting: AirPlay 2 is on when
 * airplay2 is true or when AUDIOSYNC_AIRPLAY2=1 is set; off, the sender makes
 * no AirPlay 2 traffic.
 */
AscAirPlay *asc_airplay_create_with_airplay2(bool airplay2);

/*
 * Stops the sender and frees it: every speaker is torn down and its threads
 * end within about 2 s, which this call waits for, so call it off the main
 * thread. Takes the address of the host's pointer and sets that pointer to
 * NULL, so destroying through it again does nothing; a NULL address or
 * pointer does nothing. An input taken from it stays valid until its own
 * destroy.
 */
void asc_airplay_destroy(AscAirPlay **airplay);

/*
 * Starts browsing the local network for AirPlay receivers over multicast DNS;
 * they arrive through asc_airplay_next_receiver. Returns false when the handle
 * is NULL or dead or multicast DNS can't start.
 */
bool asc_airplay_start_discovery(AscAirPlay *airplay);

/*
 * Stops browsing; every receiver discovery listed is then reported gone.
 * Speakers keep playing.
 */
void asc_airplay_stop_discovery(AscAirPlay *airplay);

/*
 * Takes the next change discovery saw, if one is waiting, into *receiver and
 * returns true. Returns false, leaving *receiver as it was, when none is
 * waiting or either pointer is NULL. Never waits. Speaker and timeline
 * events wait for asc_airplay_next_event.
 */
bool asc_airplay_next_receiver(AscAirPlay *airplay, AscAirPlayReceiver *receiver);

/*
 * Takes the next speaker or timeline event, if one is waiting, into *event
 * and returns true. Returns false, leaving *event as it was, when none is
 * waiting or either pointer is NULL. Never waits. Receiver changes wait for
 * asc_airplay_next_receiver, and an event waits until every receiver change
 * queued before it was taken there: take the changes first. A dead sender
 * reports ASC_AIRPLAY_EVENT_FAILED once, in place of whatever still waited,
 * and nothing after it.
 */
bool asc_airplay_next_event(AscAirPlay *airplay, AscAirPlayEvent *event);

/*
 * Adds the receiver discovery lists under `id` as a speaker, at slider
 * position `volume` (0 to 100) times the master, held back by the hold-back
 * set last (asc_airplay_set_hold_back): a session starts at once, and its
 * state, errors and latency come as events.
 * It uses the password handed over for `id` and the stored credentials, if
 * any. Adding one already added applies the volume, the password and the
 * credentials and, if it failed or waits for
 * a retry, starts a new session now. Returns false, adding nothing, when the
 * handle or id is NULL, id isn't a listed receiver, or volume isn't finite.
 */
bool asc_airplay_add_speaker(AscAirPlay *airplay, const char *id, double volume);

/*
 * Removes the speaker added under `id`: TEARDOWN goes out and its connection
 * closes after the answer or 2 s; a stopped state event follows. Returns
 * false when the handle or id is NULL or no speaker was added under id.
 */
bool asc_airplay_remove_speaker(AscAirPlay *airplay, const char *id);

/*
 * Sets the slider position (0 to 100) of the speaker added under `id`; it
 * plays at that position times the master, live. Returns false, changing
 * nothing, when the handle or id is NULL, no speaker was added under id, or
 * volume isn't finite.
 */
bool asc_airplay_set_volume(AscAirPlay *airplay, const char *id, double volume);

/*
 * Sets the master position (0 to 100), which scales every speaker's volume,
 * live, and that of every speaker added later; a master that isn't finite is
 * ignored. A speaker plays at the product of both positions, spread evenly
 * over -30 to 0 dB, and muted at 0.
 */
void asc_airplay_set_master(AscAirPlay *airplay, double master);

/*
 * Holds every speaker back by hold_back_ms on top of L, live, and every
 * speaker added later, so they meet an output that plays later than L, such
 * as a Google Cast speaker with a longer offset. The sender caps a speaker's
 * delay at ASC_AIRPLAY_MAX_DELAY_MS; past it the speakers play early by the
 * rest. A value below 0 or not finite counts as 0. Returns the hold-back
 * applied, in ms; 0 for a NULL or dead handle.
 */
double asc_airplay_set_hold_back(AscAirPlay *airplay, double hold_back_ms);

/*
 * The base latency L in milliseconds: a local output lines up with the
 * speakers at L plus its own delay. 0 for a NULL or dead handle.
 */
double asc_airplay_latency_ms(const AscAirPlay *airplay);

/*
 * The host restarted its capture: every speaker is flushed and the timeline
 * starts over; a timeline event follows.
 */
void asc_airplay_restart_timeline(AscAirPlay *airplay);

/*
 * A speaker's counters, as asc_airplay_speaker_counters copies them: one
 * session's, or a stereo pair's or home theater group's summed over its
 * members. They keep counting across the speaker's reconnects.
 */
typedef struct AscAirPlayCounters {
    uint64_t packets_sent;
    uint64_t resend_requests;
    uint64_t packets_resent;
    uint64_t not_held;             /* requested packets not resent: no longer held, or resent three times */
    uint64_t timing_replies;
    uint64_t late_packets;         /* sent more than 5 ms after their time */
    uint64_t silent_frames;
    uint64_t input_frames_dropped; /* the stream's count: every session plays the one input */
    uint64_t foreign_datagrams;    /* dropped on the control and timing sockets: from another address */
    uint32_t sessions;             /* sessions summed: 1 for a speaker, the member count for a target */
} AscAirPlayCounters;

/* The stream's counters, as asc_airplay_stream_counters copies them. */
typedef struct AscAirPlayStreamCounters {
    double rate_hz;                /* the current input rate estimate */
    uint64_t packets_encoded;      /* one per packet of the timeline */
    uint64_t silent_frames;        /* inserted where the input hadn't arrived in time */
    uint64_t input_frames_dropped;
} AscAirPlayStreamCounters;

/*
 * Copies into *out the counters of the speaker added under id: a stereo
 * pair's or home theater group's summed over its members, or one member's
 * when id names a member of an added target. Reads atomics only, never
 * blocking the sender. False, with *out zeroed, when the handle or id is
 * NULL, the sender stopped working, or nothing was added under id; false
 * and nothing written when out is NULL.
 */
bool asc_airplay_speaker_counters(const AscAirPlay *airplay, const char *id,
                                  AscAirPlayCounters *out);

/*
 * Copies the stream's counters into *out. False, with *out zeroed, for a
 * NULL handle or a sender that stopped working; false and nothing written
 * when out is NULL.
 */
bool asc_airplay_stream_counters(const AscAirPlay *airplay, AscAirPlayStreamCounters *out);

/*
 * Sets what is playing, for the receivers that show it: the track's title,
 * artist and album as UTF-8 text, and its artwork as a JPEG of at most
 * 512 KiB. Every speaker that lists the type gets it now, and every speaker
 * that starts or restarts later gets it too. A NULL text is an empty one,
 * and three NULL texts send no track information; NULL artwork, or a length
 * of 0, sends none. Returns false, changing nothing, for a NULL or dead
 * handle, a text that isn't UTF-8, a NULL artwork with a length, or artwork
 * the sender refuses (not a JPEG, or too large).
 */
bool asc_airplay_set_now_playing(AscAirPlay *airplay, const char *title, const char *artist,
                                 const char *album, const uint8_t *artwork, size_t artwork_len);

/*
 * Opens the pairing store kept in the file at `path` (UTF-8), meant for
 * once, before discovery starts: an Apple TV it holds credentials for is
 * listed as available and verifies with them; one it doesn't is listed as
 * ASC_AIRPLAY_NEEDS_PIN. The file is versioned, mode 0600, written through a
 * temporary file created new and renamed over it, and removed once nothing
 * is paired; it never holds a password. Listed receivers are reported again
 * as changed. Returns how many receivers it holds credentials for, or an
 * ASC_AIRPLAY_PAIRINGS_* code.
 */
int32_t asc_airplay_open_pairings(AscAirPlay *airplay, const char *path);

/*
 * Opens the AirPlay 2 identity kept in the file at `path` (UTF-8), meant for
 * once, before discovery starts: the device ID the AirPlay 2 path sends, made
 * and written once when the file is missing, and the HomeKit credentials of
 * the receivers it paired with, which later sessions verify with; new ones
 * are written to it as pairings give them. Kept like the pairings file
 * (versioned, mode 0600, replaced through a temporary file); it never holds
 * a password and its credentials never cross this interface. With the hidden
 * AirPlay 2 switch off it reads and writes nothing and returns 0. Returns how
 * many receivers it holds credentials for, or an ASC_AIRPLAY_PAIRINGS_* code.
 */
int32_t asc_airplay_open_airplay2_identity(AscAirPlay *airplay, const char *path);

/*
 * Starts a PIN pairing with the listed receiver `id` (one listed as
 * ASC_AIRPLAY_NEEDS_PIN that the user turned on; never on the app's own): it
 * is asked to show a PIN, and ASC_AIRPLAY_EVENT_PIN_NEEDED follows, or how
 * the pairing ended. It uses the password handed over for `id`, if any. A
 * pairing already running for it is replaced (cancelled). A new pairing
 * starts once the one cancelled before it has ended, and nothing that one
 * reports reaches the host or ends the new one; when that one ended paired
 * after all, the new one never starts and ASC_AIRPLAY_EVENT_PAIRED follows
 * instead, with no PIN asked. Returns false, starting nothing, when the
 * handle or id is NULL or id isn't listed.
 */
bool asc_airplay_start_pairing(AscAirPlay *airplay, const char *id);

/*
 * Hands the PIN the user typed to the pairing with `id`; the outcome comes as
 * an event (ASC_AIRPLAY_EVENT_PAIRED, or _PAIRING_FAILED with
 * ASC_AIRPLAY_ERROR_WRONG_PIN). Returns false, sending nothing, when a
 * pointer is NULL, no pairing runs for id, or pin isn't digits only. Never
 * kept or logged.
 */
bool asc_airplay_submit_pin(AscAirPlay *airplay, const char *id, const char *pin);

/*
 * Stops the pairing with `id`, if one runs; nothing more is reported for it.
 * Credentials it still gets before it stops are stored all the same.
 */
void asc_airplay_cancel_pairing(AscAirPlay *airplay, const char *id);

/*
 * Hands over the password of receiver `id` for its next sessions
 * (asc_airplay_add_speaker adds or restarts one) and pairings, until the
 * handle is destroyed; NULL or "" forgets it. Kept in memory only and wiped
 * when dropped, never stored or logged, and forgotten when the receiver turns
 * it down. Returns false when the handle or id is NULL.
 */
bool asc_airplay_set_password(AscAirPlay *airplay, const char *id, const char *password);

/*
 * Forgets the stored credentials of `id` and rewrites the pairings file; the
 * receiver is reported again as ASC_AIRPLAY_NEEDS_PIN. Returns whether it was
 * paired. The core also forgets credentials a receiver turns down by itself.
 */
bool asc_airplay_forget_pairing(AscAirPlay *airplay, const char *id);

/*
 * The sender's audio input, for the capture thread: the first call hands it
 * over, every later one returns NULL, as does a NULL handle. It starts
 * without a rate, so its writes are dropped until asc_airplay_input_set_rate
 * gives it one.
 */
AscAirPlayInput *asc_airplay_take_input(AscAirPlay *airplay);

/*
 * Any thread: the rate in Hz of the audio the capture writes from now on,
 * rounded to whole hertz. The next write takes it in and starts the
 * resampler afresh, with a 20 ms fade-in from silence, so call it at each
 * capture start, even at an unchanged rate. Returns false, changing nothing,
 * for a NULL handle or a rate outside 8000 to 384 000 Hz. Real-time safe.
 */
bool asc_airplay_input_set_rate(AscAirPlayInput *input, double rate);

/*
 * Any thread, at a session stop: the input fades out over the next 20 ms of
 * frames written and hands zeros after them, until the next
 * asc_airplay_input_set_rate fades it in again. Does nothing for a NULL
 * handle. Real-time safe.
 */
void asc_airplay_input_request_fade_out(AscAirPlayInput *input);

/*
 * Capture thread: hands `frames` interleaved stereo frames at the rate last
 * set to the sender, resampled to 44.1 kHz, as audio that arrived now.
 * Returns the 44.1 kHz frames handed over, 0 before a rate is set; the
 * sender drops those that come too late or don't fit. Reads the monotonic
 * clock once. Real-time safe. Returns 0 once a write panicked (only a panic
 * itself would allocate).
 */
size_t asc_airplay_input_write(AscAirPlayInput *input, const float *samples, size_t frames);

/*
 * Frees an input handle. Takes the address of the host's pointer and sets
 * that pointer to NULL, so destroying through it again does nothing; a NULL
 * address or pointer does nothing.
 */
void asc_airplay_input_destroy(AscAirPlayInput **input);

/* ---- Google Cast sender (AudioSync's own, cast/) --------------------------- */

/*
 * AudioSync's own Google Cast sender: discovery, one device's session at a
 * time, its events, the device memory the host stores between runs, and
 * the capture's audio through an input handle. Devices go by their id: 32
 * hex digits from discovery, or "manual:<ip:port>" for one added by
 * address. A Cast device is a measured-offset output: nothing ties its
 * playback to a clock, so the host delays its other outputs by the offset
 * stored here per device, and Resync is only ever a user action.
 * Every asc_cast_* call on a sender handle runs on one control thread at a
 * time, never an audio thread, and none waits on the network: the sender's
 * worker thread makes every network call. asc_cast_destroy is the one call
 * that waits. An input handle (AscCastInput) has its own rules, below.
 *
 * A bug in the sender never ends the host: a panic inside a call returns
 * that call's failure value (NULL, false, 0 or -1) and makes the handle
 * dead, as does a panic that ends its worker thread. A dead handle's calls
 * do nothing, except that asc_cast_next_event reports ASC_CAST_EVENT_FAILED
 * once; the host then destroys it and may create a new one. Its .detail is
 * "panic at <file>:<line>", "latest panic at <file>:<line>" or "", as for
 * the AirPlay sender.
 */
typedef struct AscCast AscCast;

/*
 * The capture's way into a Cast sender (asc_cast_take_input). Its write runs
 * on one capture thread at a time, its rate and fade-out requests on any
 * thread, and its destroy once no write can run. It may outlive its sender,
 * whose audio then goes nowhere.
 */
typedef struct AscCastInput AscCastInput;

/*
 * One device change. The strings are NUL-terminated UTF-8 that the handle
 * owns, valid until the next asc_cast_next_device call on it or its destroy.
 * A gone device has only its id; its name and model are "", its flags 0.
 */
typedef struct AscCastDevice {
    int32_t event;     /* ASC_CAST_APPEARED, _CHANGED or _GONE */
    const char *id;    /* 32 hex digits, or "manual:<ip:port>" */
    const char *name;  /* the name it announces, "" when none */
    const char *model; /* its model, "" when none */
    uint32_t flags;    /* ASC_CAST_FLAG_* bits */
} AscCastDevice;

/*
 * One member of the playing Cast group. The strings are NUL-terminated
 * UTF-8 that the handle owns, valid until the next asc_cast_member call on
 * it or its destroy.
 */
typedef struct AscCastMember {
    const char *id;   /* the listed device it is, as AscCastDevice.id; "" when none matches: show the name alone */
    const char *name; /* the name the group gives it, "" when none */
} AscCastMember;

/*
 * One thing that happened to an attempt, or to the sender. The strings are
 * NUL-terminated UTF-8 that the handle owns, valid until the next
 * asc_cast_next_event call on it or its destroy. Fields a kind doesn't use
 * are 0 or "", except retry_ms, which is then -1.
 */
typedef struct AscCastEvent {
    int32_t kind;        /* ASC_CAST_EVENT_* */
    uint64_t attempt;    /* what asc_cast_start returned for it; 0 for none */
    const char *id;      /* the attempt's device id; "" for none */
    int32_t reason;      /* an ended event's ASC_CAST_END_* */
    int32_t error;       /* ASC_CAST_ERROR_* */
    int32_t format;      /* ASC_CAST_FORMAT_* */
    int64_t retry_ms;    /* ms until the next connect attempt */
    double level;        /* the device's volume, 0 to 1 */
    bool muted;
    bool volume_fixed;   /* the device's volume can't be changed: show the control as unavailable */
    double step;         /* the device's volume step; 0 when it reports none */
    double drift_ms;     /* dropped audio plus rebuffering since the stream was loaded */
    double queued_ms;    /* audio waiting in the sender's queue */
    bool resync_offered; /* the drift is over 250 ms: the host may offer Resync */
    bool relaunched;     /* the app had to be launched and loaded again */
    const char *detail;  /* an error's text, a log line, a panic location; for a debug log */
} AscCastEvent;

/*
 * Starts a Cast sender's worker thread with the device memory the host
 * stored from asc_cast_memory_json; NULL, or text that doesn't read, starts
 * empty, and damaged entries are skipped. Discovery waits for
 * asc_cast_start_discovery. Returns NULL when the thread can't start.
 */
AscCast *asc_cast_create(const char *memory_json);

/*
 * Stops the sender and frees it: a running or starting session is torn down
 * within 3 s, which this call waits for (about 3.5 s at most), so call it
 * from the quit path, off the main thread. Takes the address of the host's
 * pointer and sets that pointer to NULL; a NULL address or pointer does
 * nothing. An input taken from it stays valid until its own destroy.
 */
void asc_cast_destroy(AscCast **cast);

/*
 * Starts browsing the local network for Cast devices over multicast DNS;
 * they arrive through asc_cast_next_device. True when it runs already.
 * Returns false when the handle is NULL or dead or multicast DNS can't start.
 */
bool asc_cast_start_discovery(AscCast *cast);

/*
 * Stops browsing: every device discovery found is reported gone, except one
 * in a session, which stays listed as offline. Devices added by address stay.
 */
void asc_cast_stop_discovery(AscCast *cast);

/*
 * Adds a device by address, for a network where multicast DNS is blocked:
 * "ip:port", "[ipv6]:port" or an address alone (port 8009). It is listed as
 * "manual:<ip:port>" until a browse finds its id, then reported gone and
 * listed again under that id. Returns false for a NULL or dead handle or an
 * address that doesn't read.
 */
bool asc_cast_add_device(AscCast *cast, const char *address);

/*
 * Takes the next device change, if one is waiting, into *device and returns
 * true. Returns false, leaving *device as it was, when none is waiting or
 * either pointer is NULL. Never waits.
 */
bool asc_cast_next_device(AscCast *cast, AscCastDevice *device);

/*
 * Takes the next event, if one is waiting, into *event and returns true.
 * Returns false, leaving *event as it was, when none is waiting or either
 * pointer is NULL. Never waits. Every attempt's events carry its number,
 * and every attempt ends with exactly one ASC_CAST_EVENT_ENDED. A dead
 * sender reports ASC_CAST_EVENT_FAILED once, in place of whatever still
 * waited, and nothing after it.
 */
bool asc_cast_next_event(AscCast *cast, AscCastEvent *event);

/*
 * Starts a session on the listed device `id` in the background and returns
 * its attempt number (1 or more). The stream plays what the input writes
 * from then on, with the metadata and artwork set last; the format that
 * worked last on the device is tried first. Returns 0, starting nothing,
 * for a NULL or dead handle, an id that isn't listed, or while
 * another Cast device plays or starts; *refused (when refused isn't NULL)
 * gets the ASC_CAST_REFUSED_* code, ASC_CAST_REFUSED_NONE on success.
 */
uint64_t asc_cast_start(AscCast *cast, const char *id, int32_t *refused);

/*
 * How many members the playing group has, as its latest
 * ASC_CAST_EVENT_MEMBERS reported; 0 while none are known, when no group
 * plays, and for a NULL or dead handle.
 */
size_t asc_cast_member_count(AscCast *cast);

/*
 * Writes member `index` of the playing group into *member and returns true.
 * Returns false, leaving *member as it was, for an index past the count or
 * a NULL pointer or handle.
 */
bool asc_cast_member(AscCast *cast, size_t index, AscCastMember *member);

/*
 * Stops the session, or the attempt to start one: connect retries and the
 * waits for the device end at once, and the teardown runs on the worker.
 * Another device may start right away; this attempt's ended event follows.
 */
void asc_cast_stop(AscCast *cast);

/*
 * Sets the device's volume, 0 to 1, clamped. Show the level volume events
 * report, and while the user drags a slider send at most one every 100 ms.
 * Returns false, sending nothing, for a NULL or dead handle, a level that
 * isn't finite, or when no session runs or starts; a locked volume comes as
 * ASC_CAST_EVENT_COMMAND_FAILED with ASC_CAST_ERROR_VOLUME_FIXED.
 */
bool asc_cast_set_volume(AscCast *cast, double level);

/*
 * Mutes or unmutes the device, apart from its level. Returns false, sending
 * nothing, for a NULL or dead handle or when no session runs or starts.
 */
bool asc_cast_set_muted(AscCast *cast, bool muted);

/*
 * Moves the device's volume one of its own steps up or down. Returns false,
 * sending nothing, for a NULL or dead handle or when no session runs or
 * starts.
 */
bool asc_cast_step_volume(AscCast *cast, bool up);

/*
 * Sets the track's title, artist and album (UTF-8) for the next time the
 * stream is loaded: the next start, or a Resync; a playing stream is not
 * reloaded for it. NULL or "" leaves a field out. Returns false, changing
 * nothing, for a NULL or dead handle or a text that isn't UTF-8.
 */
bool asc_cast_set_metadata(AscCast *cast, const char *title, const char *artist, const char *album);

/*
 * Sets the track's artwork, a PNG or JPEG of at most 1280 by 720, for the
 * next load of this and every later session; NULL or a length of 0 forgets
 * it. A device or sender refusing the image comes as
 * ASC_CAST_EVENT_COMMAND_FAILED with ASC_CAST_ERROR_ARTWORK. Returns false,
 * changing nothing, for a NULL or dead handle, NULL bytes with a length, or
 * more than 8 MiB.
 */
bool asc_cast_set_artwork(AscCast *cast, const uint8_t *bytes, size_t len);

/*
 * The user's Resync: the stream is loaded again on the same app, which brings
 * the latency back toward where it started, at the cost of a gap. Never sent
 * by the core on its own. Returns false, sending nothing, for a NULL or dead
 * handle or when no session runs or starts.
 */
bool asc_cast_resync(AscCast *cast);

/*
 * Stores the user's measured offset for device `id` in ms, rounded to whole
 * ms; the host delays its other outputs by it. The device needn't be listed.
 * Store asc_cast_memory_json afterwards. Returns false for a NULL or dead
 * handle, an id that doesn't read, or an offset outside 0 to 120 000 ms.
 */
bool asc_cast_set_offset(AscCast *cast, const char *id, double offset_ms);

/*
 * The offset stored for device `id` in ms; -1 when none is, and for a NULL
 * or dead handle or an id that doesn't read.
 */
double asc_cast_offset_ms(AscCast *cast, const char *id);

/*
 * Holds the audio the input hands the session back by hold_back_ms, live,
 * so a device whose offset is shorter than L plays at L. It stops at
 * 20 000 ms; a value below 0 or not finite counts as 0. A change fades out
 * and in over 20 ms; a capture start (asc_cast_input_set_rate) begins with
 * silence for the hold-back, never with the end of the last capture.
 * Returns the hold-back applied, in ms; 0 for a NULL or dead handle.
 */
double asc_cast_set_hold_back(AscCast *cast, double hold_back_ms);

/*
 * The device memory to store between runs, as JSON: per device, the format
 * that last worked and the offset. NUL-terminated UTF-8 that the handle
 * owns, valid until the next asc_cast_memory_json call on it or its destroy.
 * NULL for a NULL or dead handle.
 */
const char *asc_cast_memory_json(AscCast *cast);

/*
 * The sender's audio input, for the capture thread: the first call hands it
 * over, every later one returns NULL, as does a NULL or dead handle. It
 * starts without a rate, so its writes are dropped until
 * asc_cast_input_set_rate gives it one.
 */
AscCastInput *asc_cast_take_input(AscCast *cast);

/*
 * Any thread: the rate in Hz of the audio the capture writes from now on,
 * rounded to whole hertz. The next write takes it in and starts the
 * resampler afresh, with a 20 ms fade-in from silence, so call it at each
 * capture start, even at an unchanged rate. Returns false, changing
 * nothing, for a NULL handle or a rate outside 8000 to 384 000 Hz.
 * Real-time safe.
 */
bool asc_cast_input_set_rate(AscCastInput *input, double rate);

/*
 * Any thread, at a capture stop: the input fades out over the next 20 ms of
 * frames written and hands zeros after them, until the next
 * asc_cast_input_set_rate fades it in again. Does nothing for a NULL handle.
 * Real-time safe.
 */
void asc_cast_input_request_fade_out(AscCastInput *input);

/*
 * Capture thread: hands `frames` interleaved stereo frames at the rate last
 * set to the running session, resampled to ASC_CAST_RATE. Returns the
 * frames at that rate its ring took: 0 before a rate is set and while no
 * session runs or starts; frames that don't fit are dropped. Real-time
 * safe. Returns 0 once a write panicked.
 */
size_t asc_cast_input_write(AscCastInput *input, const float *samples, size_t frames);

/*
 * Frees an input handle. Takes the address of the host's pointer and sets
 * that pointer to NULL; a NULL address or pointer does nothing.
 */
void asc_cast_input_destroy(AscCastInput **input);

/* ---- Web remote (AudioSync's own, remote/) --------------------------------- */

/*
 * AudioSync's web remote: a small HTTP server on the home network that
 * serves one page to paired phones. The host publishes a full snapshot as
 * JSON whenever something the page shows changes; each change a phone makes
 * comes back through the command callback, on a server thread, and the host
 * applies it on its main thread like a click in its window, then publishes
 * the result. The server never calls the host for a read.
 * Every asc_remote_* call on a handle runs on one control thread at a time,
 * never an audio thread. asc_remote_stop waits for a callback in progress,
 * so the callback must not wait for the thread that stops the remote.
 * A panic inside a call returns that call's failure value (NULL, false or 0).
 */
typedef struct AscRemote AscRemote;

/*
 * The host's command callback, on a server thread, one call at a time:
 * command_json is NUL-terminated UTF-8 valid during the call only, either
 * {"type":"speaker","id":...} with the posted "on", "volume" (0 to 100) and
 * "delay_ms" among the fields, or {"type":"master","volume":...}. Every
 * value was checked against the last snapshot's ranges.
 */
typedef void (*AscRemoteCommand)(void *context, const char *command_json);

/*
 * Starts the remote. config_json is {"port", "local_name", "devices_path",
 * "listen"}, each optional: the port (default ASC_REMOTE_DEFAULT_PORT), the
 * computer's name so <local_name>.local is an accepted address, the file
 * that keeps the paired devices' token hashes (none: memory only), and the
 * IP literals to listen on (default every IPv6 and IPv4 address). command
 * gets each accepted change with context; both may be used from any thread
 * until asc_remote_stop returns. Returns NULL on failure with the reason
 * (ASC_REMOTE_ERROR_*) in *error when error isn't NULL.
 */
AscRemote *asc_remote_start(const char *config_json, AscRemoteCommand command, void *context, int32_t *error);

/* The port the remote listens on. */
uint32_t asc_remote_port(AscRemote *remote);

/*
 * Publishes the snapshot in state_json: {"computer_name", "master": {"volume"},
 * "now_playing": {"title", "artist", "source"} or null, "speakers": [{"id",
 * "name", "kind", "on", "volume", "delay_ms", "delay_min_ms", "delay_max_ms",
 * "available", "notice"}]}, volumes 0 to 100, ids unique; computer_name is
 * the name the page's footer shows, empty when left out. The server numbers
 * it and streams it to the open pages; an unchanged snapshot sends nothing.
 * Returns false when it doesn't read; the previous one then stays. Neither
 * the snapshot nor the name ever reaches the log.
 */
bool asc_remote_publish(AscRemote *remote, const char *state_json);

/*
 * The pairing code that works now, 6 digits, or NULL when none does (it
 * expired after 10 minutes: call asc_remote_rotate_code). The string is the
 * handle's, valid until the next code call on it or its stop.
 */
const char *asc_remote_pairing_code(AscRemote *remote);

/*
 * Makes a new pairing code, for when the user opens the setting: the old
 * one stops working and a lockout after too many wrong codes ends. Returns
 * it as asc_remote_pairing_code does; NULL when no code could be made.
 */
const char *asc_remote_rotate_code(AscRemote *remote);

/* Whether 20 wrong codes locked pairing until the next asc_remote_rotate_code. */
bool asc_remote_pairing_locked(AscRemote *remote);

/* How many devices are paired (8 at most; pairing a ninth drops the oldest). */
uint32_t asc_remote_device_count(AscRemote *remote);

/*
 * Forgets every paired device: their next request is refused and their open
 * pages lose the stream. Returns false when the device file couldn't be
 * written; they are still forgotten until the app quits.
 */
bool asc_remote_forget_all(AscRemote *remote);

/*
 * The oldest debug-log line not taken yet (pairing, refusals, file
 * problems; never a code or a token), or NULL when there is none. The
 * string is the handle's, valid until the next call of this on it or its stop.
 */
const char *asc_remote_next_log(AscRemote *remote);

/*
 * Stops the remote and frees it: closes the port and every connection and
 * waits for a command callback in progress; once it returns the callback is
 * never called again. Takes the address of the host's pointer and sets that
 * pointer to NULL; a NULL address or pointer does nothing.
 */
void asc_remote_stop(AscRemote **remote);

#ifdef __cplusplus
}
#endif

#endif /* AUDIOSYNC_CORE_H */
