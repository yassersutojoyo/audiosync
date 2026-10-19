/*
 * audiosync_core.h: the C interface of audiosync-core, AudioSync's
 * platform-independent audio core (MIT).
 *
 * Audio crosses this interface one whole buffer at a time: a pointer to
 * interleaved stereo float32 frames (L, R) plus a frame count, never one
 * sample per call. The core owns no thread, device, file or pipe: the host
 * captures, plays, opens the pipe and runs every thread, and calls in here.
 * The one exception is AudioSync's own AirPlay sender (asc_airplay_*), which
 * runs its own threads and network sockets.
 *
 * Real-time calls (marked "Real-time safe") never allocate, free, lock, wait,
 * log or make a system call, and never panic: bad arguments make them do
 * nothing or saturate.
 *
 * Threads. Each handle lists which thread runs which call:
 * - an audio call (capture thread, output thread or FIFO writer thread) runs
 *   on one thread at a time per handle;
 * - a control call runs on one control thread at a time per handle, and may
 *   run while that handle's audio call runs;
 * - an "any thread" call is a lock-free atomic operation and may run anywhere,
 *   at any time, while the handle lives;
 * - a destroy call runs once no other call on that handle can run.
 * Handles that share audio (a ring and its writer and readers, a FIFO feed
 * and its writer) are reference-counted inside, so they may be destroyed in
 * any order. Functions without a handle are thread-safe.
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

/* The pipe's format: 48 kHz, signed 16-bit little-endian stereo, 4 bytes a frame. */
#define ASC_PIPE_RATE 48000
#define ASC_PIPE_FRAME_BYTES 4

/* The most bytes one write callback is offered: the smallest PIPE_BUF POSIX allows. */
#define ASC_PIPE_WRITE_MAX_BYTES 512

/* The usual FIFO queue length in seconds. */
#define ASC_FIFO_DEFAULT_QUEUE_SECONDS 4

/* What a pipe callback reports (AscPipeOpenFn, AscPipeWriteFn). */
#define ASC_PIPE_OK 0          /* opened, or wrote *written bytes */
#define ASC_PIPE_NO_READER 1   /* open: no process reads the FIFO yet (ENXIO) */
#define ASC_PIPE_INTERRUPTED 2 /* open or write: interrupted by a signal (EINTR) */
#define ASC_PIPE_WOULD_BLOCK 3 /* write: the pipe is full (EAGAIN) */
#define ASC_PIPE_BROKEN 4      /* write: the reader went away (EPIPE) */
#define ASC_PIPE_FAILED 5      /* open or write: any other error */

/* What an AirPlay receiver event reports (AscAirPlayReceiver.event). */
#define ASC_AIRPLAY_APPEARED 1 /* discovery found a receiver */
#define ASC_AIRPLAY_CHANGED 2  /* its records or its verdict changed */
#define ASC_AIRPLAY_GONE 3     /* it left the network, or discovery stopped */

/* Whether a receiver can play from this sender (AscAirPlayReceiver.availability). */
#define ASC_AIRPLAY_AVAILABLE 0      /* it can */
#define ASC_AIRPLAY_NEEDS_PIN 1      /* an Apple TV that asks for a PIN once */
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

/* What discovery saw (AscAirPlayReceiver.flags), bit by bit. */
#define ASC_AIRPLAY_FLAG_RAOP 1            /* announces _raop._tcp (AirPlay 1 audio) */
#define ASC_AIRPLAY_FLAG_AIRPLAY 2         /* announces _airplay._tcp */
#define ASC_AIRPLAY_FLAG_APPLE_TV 4        /* its model is an Apple TV */
#define ASC_AIRPLAY_FLAG_PASSWORD 8        /* its records say it needs a password */
#define ASC_AIRPLAY_FLAG_PAIRING 16        /* its flags hint that it needs pairing */
#define ASC_AIRPLAY_FLAG_LEGACY_PAIRING 32 /* an Apple TV that needs the PIN pairing */
#define ASC_AIRPLAY_FLAG_BUSY 64           /* its flags hint that another sender plays */
#define ASC_AIRPLAY_FLAG_AIRPLAY2 128      /* it lists AirPlay 2 features */

/* What a sender event reports (AscAirPlayEvent.kind). */
#define ASC_AIRPLAY_EVENT_STATE 1    /* a speaker's state changed: .state */
#define ASC_AIRPLAY_EVENT_ERROR 2    /* an error ended a speaker's session: .error */
#define ASC_AIRPLAY_EVENT_LATENCY 3  /* a speaker's latencies: .latency_ms, .receiver_latency_frames */
#define ASC_AIRPLAY_EVENT_TIMELINE 4 /* the base latency L or the timeline changed: .latency_ms */
#define ASC_AIRPLAY_EVENT_FAILED 5   /* the sender stopped working (a bug); destroy it, create a new one */

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
 * Capture thread: appends `frames` stereo frames. Any count is accepted; once
 * the ring is full the oldest frames are overwritten, and a write larger than
 * the ring keeps only its last frames. Never waits for readers. Real-time safe.
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
 * delay starts at 0 (one block) and its drift tolerance at 35 ms; it anchors
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
 * and infinity included; the output stage and PCM16 conversion deal with
 * those. Real-time safe.
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

/* ---- FIFO feed: PCM16 for the AirPlay engine's pipe --------------------- */

/*
 * Converts `samples` floats to signed 16-bit: NaN gives 0, anything else
 * round(clamp(x, -1, 1) x 32767), ties away from zero. Returns the count
 * converted. Real-time safe.
 */
size_t asc_convert_to_pcm16(const float *input, int16_t *output, size_t samples);

/*
 * The pipe as the host's FIFO writer thread implements it with POSIX calls.
 * Every callback gets the sink's context. The core calls them only from
 * asc_fifo_writer_step and asc_fifo_writer_close_step, on the calling thread.
 */

/* Opens the pipe write-only and non-blocking: ASC_PIPE_OK, NO_READER, INTERRUPTED or FAILED. */
typedef int32_t (*AscPipeOpenFn)(void *context);

/*
 * Writes up to `length` bytes (1...ASC_PIPE_WRITE_MAX_BYTES, whole frames)
 * from `bytes`: ASC_PIPE_OK with the count in *written (a short write is
 * fine), or WOULD_BLOCK, INTERRUPTED, BROKEN or FAILED.
 */
typedef int32_t (*AscPipeWriteFn)(void *context, const uint8_t *bytes, size_t length, size_t *written);

/* Closes the descriptor the open callback opened. */
typedef void (*AscPipeCloseFn)(void *context);

/* The host's pipe: a context pointer and three callbacks (a NULL callback fails). */
typedef struct AscPipeSink {
    void *context;
    AscPipeOpenFn open;
    AscPipeWriteFn write;
    AscPipeCloseFn close;
} AscPipeSink;

/*
 * The FIFO feed's counters: monotonic over the feed's life, read without
 * blocking anyone; starting and stopping a writer resets nothing.
 */
typedef struct AscFifoCounters {
    uint64_t opens;                 /* successful opens of the pipe */
    uint64_t write_errors;          /* failed writes other than a full pipe */
    uint64_t backlog_skips;         /* times the 250 ms backlog cap or an open dropped audio */
    uint64_t backlog_skip_samples;  /* 16-bit samples those drops removed */
    uint64_t pipe_full_waits;       /* waits on a full pipe */
    uint64_t short_writes;          /* writes that took only part of the bytes */
    uint64_t bytes_written;         /* bytes the pipe accepted */
    uint64_t open_failures;         /* failed opens, mostly while no reader exists */
    uint64_t bytes_abandoned;       /* bytes of a partly written frame given up */
    uint64_t samples_queued;        /* 16-bit samples asc_fifo_feed queued (frames x 2) */
    uint64_t frames_dropped;        /* 48 kHz frames asc_fifo_feed dropped on a full queue */
} AscFifoCounters;

/*
 * The FIFO feed: converts captured audio to 48 kHz PCM16 into a lock-free
 * queue. asc_fifo_set_input_rate and asc_fifo_feed run on the capture
 * thread, or on the control thread while capture is stopped; the other
 * calls run on any thread.
 */
typedef struct AscFifoFeed AscFifoFeed;

/*
 * The feed's single writer, driven by the host's writer thread: each step
 * does one bounded piece of work through the host's AscPipeSink.
 */
typedef struct AscFifoWriter AscFifoWriter;

/*
 * Creates a feed whose queue holds queue_seconds x 48 000 frames, converting
 * from 48 kHz until asc_fifo_set_input_rate. NULL outside 1...3600 seconds.
 */
AscFifoFeed *asc_fifo_create(uint32_t queue_seconds);

/* Destroys a feed handle; a writer still alive keeps the queue alive. */
void asc_fifo_destroy(AscFifoFeed *feed);

/*
 * At each session start, before the first asc_fifo_feed: sets the capture
 * rate and resets the converter. False outside 8000...384000 Hz. Never
 * allocates.
 */
bool asc_fifo_set_input_rate(AscFifoFeed *feed, uint32_t rate);

/*
 * Capture thread: converts `frames` stereo frames to 48 kHz PCM16 and queues
 * them, dropping what doesn't fit. Never touches the pipe; works whether or
 * not a writer runs. Returns the 48 kHz frames queued. Real-time safe.
 */
size_t asc_fifo_feed(AscFifoFeed *feed, const float *interleaved, size_t frames);

/* Any thread: a snapshot of the counters; fields may come from different moments. */
AscFifoCounters asc_fifo_counters(const AscFifoFeed *feed);

/*
 * Control thread, at a writer start: takes the queue's only consumer and
 * empties the queue, so writing starts with what is fed next. NULL while
 * another writer exists.
 */
AscFifoWriter *asc_fifo_writer_create(AscFifoFeed *feed);

/*
 * Destroys a writer, so the feed can hand out a new one. Close the pipe with
 * asc_fifo_writer_close_step first; this calls no callback.
 */
void asc_fifo_writer_destroy(AscFifoWriter *writer);

/*
 * Writer thread: one bounded piece of work. It keeps the unwritten audio at
 * or under 250 ms (dropping the oldest whole frames), opens the pipe while it
 * is closed (each open, the first or a reopen, drops whatever queued before
 * it, so writing starts at the live edge), otherwise makes one write of at
 * most ASC_PIPE_WRITE_MAX_BYTES that ends on a frame boundary, resumes a
 * short write at the next byte and reopens after a broken pipe on a frame
 * boundary. Returns the microseconds to sleep before the next step (or until
 * the host wakes the thread to stop); 0 means step again at once. A NULL
 * writer or sink does nothing and returns the open retry pause.
 */
uint32_t asc_fifo_writer_step(AscFifoWriter *writer, const AscPipeSink *sink);

/*
 * Writer thread, once stopping: one step of closing the pipe on a frame
 * boundary. Returns true while a frame the reader got only part of is still
 * being completed: sleep *pause_us microseconds (pause_us may be NULL) and
 * call again. Returns false once the pipe is closed; after about 100 ms of
 * pauses the rest of that frame is given up, so a stop stays well under
 * 300 ms. The writer may step again afterwards.
 */
bool asc_fifo_writer_close_step(AscFifoWriter *writer, const AscPipeSink *sink, uint32_t *pause_us);

/* ---- AirPlay sync rule (local output delays) ----------------------------- */

/*
 * Inputs of the AirPlay sync rule. IDs are AirPlay output IDs as
 * NUL-terminated UTF-8, compared whole; a NULL array (with any count), a NULL
 * entry or an entry that isn't valid UTF-8 counts as no ID.
 */
typedef struct AscSyncInput {
    const char *const *enabled_ids;  /* AirPlay outputs the user enabled */
    size_t enabled_count;
    const char *const *listed_ids;   /* the AirPlay engine's latest output list */
    size_t listed_count;
    bool was_chosen;                 /* airplay_chosen of the previous decision */
} AscSyncInput;

/* The sync rule's decision. */
typedef struct AscSyncDecision {
    bool airplay_chosen;  /* some enabled AirPlay output is listed now */
    bool flipped;         /* airplay_chosen changed: give every running local output its new effective delay */
} AscSyncDecision;

/* Decides whether an AirPlay speaker is chosen, and whether that flipped. A NULL input gives all false. */
AscSyncDecision asc_sync_decide(const AscSyncInput *input);

/* A local output's effective delay: its configured delay while an AirPlay speaker is chosen, otherwise 0. */
double asc_sync_effective_delay_ms(double configured_ms, bool airplay_chosen);

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

/* ---- AirPlay sender (AudioSync's own, airplay/) ----------------------------- */

/*
 * AudioSync's own AirPlay sender: discovery, speakers added by the IDs
 * discovery reports, their volume and the master, their events, and the
 * capture's audio through an input handle. Nothing secret crosses this
 * interface (no password, PIN, key or credential); PIN pairing comes later.
 * Every asc_airplay_* call on a sender handle runs on one control thread at
 * a time, never an audio thread; the sender runs its own threads and
 * sockets. An input handle (AscAirPlayInput) has its own rules, below.
 *
 * A bug in the sender never ends the host: a panic inside a call returns
 * that call's failure value (NULL, false or 0) and makes its handle dead,
 * as does a panic that ends the sender's own driver thread. A dead handle's
 * calls do nothing and return their failure values, except that
 * asc_airplay_next_event reports ASC_AIRPLAY_EVENT_FAILED once; the host
 * then destroys it (which still frees it) and may create a new sender. A
 * panic in an input write makes the input and its sender dead.
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
    const char *detail;              /* an error event's error, for a debug log (no secrets) */
} AscAirPlayEvent;

/*
 * Starts an AirPlay sender: its stream, silent until audio comes through its
 * input (asc_airplay_take_input), and its driver thread; discovery waits for
 * asc_airplay_start_discovery. The master starts at 100. Returns NULL when
 * the thread can't start.
 */
AscAirPlay *asc_airplay_create(void);

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
 * is NULL or multicast DNS can't start.
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
 * asc_airplay_next_receiver. A dead sender reports ASC_AIRPLAY_EVENT_FAILED
 * once, in place of whatever still waited, and nothing after it.
 */
bool asc_airplay_next_event(AscAirPlay *airplay, AscAirPlayEvent *event);

/*
 * Adds the receiver discovery lists under `id` as a speaker, at slider
 * position `volume` (0 to 100) times the master, with no delay of its own: a
 * session starts at once, and its state, errors and latency come as events.
 * Adding one already added applies the volume and, if it failed or waits for
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
 * The base latency L in milliseconds: a local output lines up with the
 * speakers at L plus its own delay. 0 for a NULL handle.
 */
double asc_airplay_latency_ms(const AscAirPlay *airplay);

/*
 * The host restarted its capture: every speaker is flushed and the timeline
 * starts over; a timeline event follows.
 */
void asc_airplay_restart_timeline(AscAirPlay *airplay);

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
 * resampler afresh, so call it at each capture start, even at an unchanged
 * rate. Returns false, changing nothing, for a NULL handle or a rate outside
 * 8000 to 384 000 Hz. Real-time safe.
 */
bool asc_airplay_input_set_rate(AscAirPlayInput *input, double rate);

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

#ifdef __cplusplus
}
#endif

#endif /* AUDIOSYNC_CORE_H */
