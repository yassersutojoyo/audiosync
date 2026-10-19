# audiosync-core implementation notes

Everything in this crate is written from `docs/ENGINE_SPEC.md`, whose clause IDs (`S1` and up) appear in comments and test names, and from public documentation (Rust and Swift docs, the RBJ Audio EQ Cookbook, the cargo-deny docs). No other source in this repository was used.

Where the spec leaves room, the readings below were chosen; each one satisfies every Observed clause. The crate was built in three parts, and these notes keep one section for each.

## Audio path: ring, delay reader, resampler, conversion and FIFO feed

### Ring (`ring.rs`)

- **S38, S40**: a write is stored in chunks of at most M frames, and W is published (release store, then release fence) after each chunk. The writer therefore never stores outside the slots of `[W - C, W - C + M)`, whatever the call size. A write larger than C stores every frame, and the earlier ones are overwritten, so only the last C remain. This costs O(frames) and keeps the post-copy check sound.
- **S35**: `Ring::writer()` hands out the single writer token. A second one is refused until the first is dropped.
- **S39, S134**: `reset()` only sets W to 0. The old samples stay in memory but can no longer be read. It is a method of the writer token, so nothing can reset the ring while a capture holds the token.

### Reader (`reader.rs`)

Implements S41 to S59 with the counters of S158 and the prime reset of S159. A reader more than T ahead steps back even while W stands still (S52, S53), unless it is dry: a reader at W makes no correction until the writer writes again.

- **S41, S43, S133**: there are two entry points. `read(dst, N, D, T)` is S41's interface. `read_target(dst, N)` reads at the control's target. The reader keeps the delay it applies: it loads the target on every read while it is not anchored, and afterwards only on the read that applies a resync request. The flag is taken with an acquire exchange before the target is loaded, so a flagged change never uses a stale target, and a target stored just before its flag waits for the flag instead of showing up as a drift correction (S46, S158).
- **S42**: N is clamped to `dst.len() / 2`. De is also clamped to at least 0, so an absurd N larger than `C - M` cannot make it negative.
- **S44**: step 2 decides in this order: prime (or wait), dry, resync, then drift (overrun, skip forward, step back). A read that waits to prime or is dry outputs silence and ends there, leaving P, F and the resync flag as they are; a prime, a resync and every drift correction set `P = W - De`.
- **S44, S45, S133**: a reader that is not anchored takes the resync flag before it loads the target, so the target it primes with is never older than a flagged one. A read that then finds `W <= De` waits and raises the flag again, which leaves it as it was for the read that primes. The waiting read's block is reported as masked, so `masked + copied + starved = N` holds for every read; it counts nothing (S158).
- **S44, S46, S158**: a dry read (`P >= W`) is a starved render of N frames, with `lagFrames = 0` and `e = W - P - De` in its report. It neither takes a pending resync nor corrects drift, so a delay change made while the writer is stopped and the reader is at W is applied by the first read after the writer writes again, and is exact from there.
- **S44, S55**: the overrun test is `P < oldest` while `oldest = W - C + M` is above 0, so only frames that were really overwritten count. A negative P left by a resync early in a session reads silence (step 3) and follows the dead-band rules instead of re-anchoring on every read.
- **S40, S55**: when the post-copy check fails, the copied range is replaced by silence and the next read re-anchors at `W - De` as an overrun. F still moves past the rejected range, because those frames are gone.
- **S158**: an overrun counts as a drift skip of e frames (e is always positive there). One extra counter, `rejected_copies`, counts post-copy rejections.
- **S159**: `reset_prime` also clears a pending re-anchor, and keeps P, which only feeds `lagFrames` until the next prime. A read that takes both a prime reset and a resync counts only the prime.
- **S50**: T is computed in integers as `(35 × Rc + 500) / 1000`, which rounds half up (44.1 kHz gives 1544).
- **S3**: computed in f64 with `floor`. A non-finite delay returns `None` so the caller can ignore it; a negative delay counts as 0.

### Resampler (`resample.rs`)

- **S85**: a band-limited Kaiser-windowed sinc (β = 9, 20 zero crossings, cutoff at 0.97 of the lower Nyquist) with weights normalized to unit DC gain. Per-phase weight banks are used when they fit in 32 Ki weights, otherwise weights come from a table interpolated per frame. Both paths give identical output.
- **S85**: the output count is exactly `ceil(fed × out / in)`, which is never more than one frame above the exact ratio (the spec allows ±2). The content is delayed by H input frames (about 0.45 ms at 44.1 kHz) so the count keeps pace with the input.
- **S94**: `set_input_rate` and `reset` never allocate. Buffers are sized when the resampler is created, for every input rate from 8 to 384 kHz.
- **S125**: a frame with a non-finite sample is stored as silence and zeroes all earlier history. A sum that overflows to a non-finite value on huge finite input is written as 0. When the rates are equal, samples pass through bit-exact (S85) and S86 maps them.

### Conversion (`convert.rs`)

- **S86**: computed in f64 so the product is exact and ties are decided on it.
- **S63-S65**: `map_to_device` is offered for the renderer's output stage. It handles channel mapping, gain, and replaces anything non-finite with 0 after the gain.

### FIFO feed (`fifo.rs`)

- **S84**: the pipe path is the host's: its `PipeSink` opens it. `samplesQueued` is monotonic over the feed's life. Neither `start` nor clearing the queue resets it.
- **S87**: the queue holds packed 32-bit frames (L in the low half, R in the high half), so a frame can never be split. A queue of zero frames is refused.
- **S88, S129, S134**: one consumer token per queue makes a second writer impossible, and `stop()` joins the thread; dropping the control stops it too. The queue is shared by reference count, so it is freed only after the writer let go of it. Writer buffers are allocated in `start()` on the caller's thread; the writer thread allocates nothing after that.
- **S89, S93**: the open is retried every 50 ms, a full pipe every 2 ms, and an empty queue is polled every 4 ms. Other write errors are handled like `EPIPE`, after one open interval.
- **S93**: each write offers at most 512 bytes (`ATOMIC_WRITE_BYTES`, the smallest `PIPE_BUF` POSIX allows and the value on macOS) and ends on a frame boundary: it runs from the next unwritten byte to 512 bytes past the first byte of that byte's frame, or to the end of the pending bytes. A non-blocking pipe takes such a write whole or refuses it with `EAGAIN`, so a reader that stalls can't leave a frame partly written, and a short write only comes from a sink that cuts writes anyway. The writer still takes up to 2048 frames from the queue at a time and sends them in these pieces, about 375 writes per second at 48 kHz.
- **S88, S93**: the AirPlay engine keeps reading the pipe across a stop and start, so a stopping writer that holds a partly written frame (left only by a sink that cuts writes, see above) first completes it. It offers only the rest of that frame (1 to 3 bytes, one atomic pipe write) every 2 ms for up to 100 ms, then closes. The rest is abandoned only after a write error or that timeout, which keeps `stop()` well under 300 ms. `clear()` keeps the rest of a frame in flight for the same reason.
- **S90**: unwritten audio means queued frames plus frames taken but not yet written. A partly written frame counts as one and is never cut. Discards remove the not-yet-started pending frames first (they are the oldest), then the queue head. The cap is enforced after every failed open attempt and before every write; a successful open drops everything instead (S169).
- **S92**: after `EPIPE`, only the rest of the frame being written is abandoned (S93). Whole frames still pending stay with the writer until the next open, which drops them (S169).
- **S169**, read literally: "every frame queued at that moment" is all the unwritten audio the writer holds when `open` returns success, the queue and the whole frames already taken from it (the same "unwritten" as S90), and it is dropped in the same step, before any write. "Each time the writer opens the pipe" includes the first open after `start()`: `start()` already empties the queue (S88), and whatever is fed between `start()` and the first successful open (while no reader exists, say) is dropped too. A partly written frame can't exist at an open, because the close before it abandoned the rest (S93); the drop keeps one anyway, as `clear()` does, so the stream could never go off a frame boundary. One or more frames dropped count as one backlog skip, two samples per frame; an open that finds nothing queued counts none. The cap is applied after a failed open attempt rather than before each attempt, so an open is the only discard in its step and counts exactly one skip. `opens` (S160) is raised only after that drop and its skip are counted: the drop's acquire load of the queue tail pairs with the producer's release store, so a thread that reads the new `opens` (a relaxed load is enough) and then feeds never has that audio dropped by the open it saw.
- **S169** test bound: "behind live" is measured as the frames fed so far minus the index of the frame written, taken when a connection's first frame is written. `tests/fifo_writer.rs` drives a writer by hand on a simulated clock (a 10 ms block every 10 ms, the writer stepping after each pause it asks for) and checks that after outages of 120 ms, 1 s and 3 s the first frame of the new connection is the first one fed after the open and no more than 50 ms further behind live than after a fresh start. A threaded test checks the same without timing: none of the audio fed before the reopen reaches the new reader. Tests that drive a writer by hand now open the pipe before feeding what they expect to see written, and threaded tests wait for the open before feeding.
- **S160**: `EINTR` counts as a write error, being "an error other than EAGAIN". A write that takes zero bytes counts as a short write and then waits like a full pipe. Two extra counters, `open_failures` and `bytes_abandoned`, follow the same rules. The producer-side `samplesQueued` (S84) and `frames_dropped` (S87) are separate accessors.

### Build

- `[profile.test] opt-level = 1`: the two-hour drift simulations then run in about a second. Debug assertions and overflow checks stay on.

## Equalizer, volume, host policies, C interface and CI

### Modules

- `eq`: the ten-band equalizer and preamp (S22 to S34), split into `EqControl` (main thread) and `EqProcessor` (capture thread) that share only atomics.
- `volume`: the cubic volume curve, host-side volume rules, volume writes coalesced per target (`LatestValues`) and the renderer's gain stage (S64, S65, S70 to S76).
- `channels`: an input device's channels to stereo (S14).
- `policy`: pure host decisions the Windows app can reuse: delays and the AirPlay sync rule; the capture source, the microphone permission and app quits; the capture check and capture rate changes; sessions, output sync, streaming and source switches; the engine lifecycle and pipe playback; the speaker list, reachability and reconcile; pairing; speaker keys, groups, device lists, layout and notices.
- `ffi`: the C interface, `ffi.rs` plus `ffi/ring.rs`, `ffi/render.rs`, `ffi/fifo.rs` and `ffi/sync.rs` (see the last part).

### C interface

Decision: a plain C ABI (`extern "C"` with `#[unsafe(no_mangle)]`) declared by the hand-written header `include/audiosync_core.h`. No UniFFI and no cbindgen.

Why:

- Real time. Audio crosses the boundary a whole buffer at a time, as a pointer plus a frame count, and is processed in place: no copy and no allocation. UniFFI passes buffers through its own serialized `RustBuffer`, which allocates and copies on every call, so it can't sit on a Core Audio callback.
- No runtime or build dependency. UniFFI needs its crate, generated bindings and a build step; cbindgen needs a tool in CI. The surface was 16 functions at first and is 71 now.
- Swift imports a C header directly through a module map, and a Windows host can use the same header or link the `rlib`.
- The header can't drift silently: a unit test in `src/ffi.rs` parses the header and the Rust definitions, maps the types, and fails when any prototype, struct, handle, callback type or `#define` differs. CI runs it with the other tests, so "the header is current" is checked without a generator.

Contract (also in the header): `asc_eq_process` runs on one capture thread at a time; the other `asc_eq_*` calls on a handle run on one control thread at a time; the two touch different fields of the handle. Null pointers make a call do nothing. All `unsafe` code of the crate is in the `ffi` module (`src/ffi.rs` and `src/ffi/`), every block with a SAFETY comment (`clippy::undocumented_unsafe_blocks` is on there); the other modules `#![forbid(unsafe_code)]`.

`ffi/smoke.sh` builds the static library and a small Swift program (`ffi/swift-smoke/`, module map plus `main.swift`) that calls the version, the volume curve, the presets, one equalizer pass, a ring with a reader, the FIFO feed and the sync rule through the header. CI runs it on macOS. It touches no audio device and no pipe.

### Real-time checks

- A counting global allocator in the test module of `src/ffi.rs` (there so that every `unsafe` line stays in the `ffi` module) proves that the equalizer (including picking up new settings while the control thread keeps publishing, and the non-finite reset path), `equalize_and_deliver`, the C process call, `GainStage`, `apply_gain` and `input_to_stereo` neither allocate nor free. `tests/realtime.rs` does the same for the ring, the reader, the resampler, the conversion, the FIFO feed and the FIFO writer.
- `tests/realtime.rs` also scans the production code of every audio-path module for locks, blocking waits, logging and file access. It reads the sources at run time and normalizes line ends, so it works on any checkout.
- The equalizer shares two coefficient sets of `AtomicU64` values plus SeqCst `active` and `reading` indices; there is no lock anywhere. Building fails on a target without native 64-bit atomics (S128).
- A unit test counts biquad steps: exactly 20 per frame (S127).
- `benches/eq.rs` (criterion): about 7.2 µs per 512-frame stereo block with all ten bands active on an Apple silicon Mac, under 0.1 % of the block's 10.7 ms at 48 kHz.

### Dependencies and licenses

- Runtime dependency: `audiosync-airplay` (`airplay/` in this repository, MIT), AudioSync's own AirPlay sender, which the macOS app reaches through this crate's C interface because it links one Rust static library; its third-party crates are listed in `THIRD_PARTY_NOTICES.md`, and `scripts/core_notices.py` writes their license texts next to the library. It raised the minimum Rust from 1.85 to 1.88, the sender's; the two clippy lints that turns on for newer std APIs (`as_chunks`, `is_multiple_of`) are allowed in `Cargo.toml` for the existing audio code.
- Dev-dependencies: `criterion` 0.7 (benchmarks) and `proptest` 1.11 (property tests). Both are MIT OR Apache-2.0.
- `deny.toml` allows only MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, Zlib, ISC and Unicode-3.0 (Unicode-DFS-2016 isn't needed). `include-dev` and `include-build` hold test, benchmark and build tools to the same list; without them cargo-deny checks only the crate itself. CI installs cargo-deny 0.20.2, the version checked locally.

### Spec readings

Where the spec left room, these readings satisfy every Observed clause:

1. S106 access kinds: a PIN flag (`needs_auth_key`) needs a code; so does a password on an AirPlay 2 speaker. The code is asked as a password when the speaker has one. An AirPlay 1 speaker with a password and no PIN flag is a legacy password speaker. This satisfies both "availability: password asks for a code" and "availability: AirPlay 1 password unsupported".
2. S98 Retry clears the start history and returns to `stopped`; the host's next start then goes to `starting` ("retry from gaveUp → starting"). A start while `failed` or `gaveUp` is refused until Retry. The sixth start within 60 s is refused at start time and moves to `gaveUp`. Exits while stopped, restarting, failed or given up are ignored.
3. S102: the engine also "came back" when an answer comes from a different engine run than the previous answer, even with no failed poll in between and even while a switch-on is in flight. `EngineLifecycle::run` counts runs; each poll carries the run it was sent to. The first answer ever counts as came back, since the engine was unreachable before it.
4. S103 settle window: measured on when the "off" report was requested: before the success, or less than 3 s after it.
5. S106 to S108 clicks: the user's click is kept until a code answers it, or a cancel, turn-off or automatic turn-on forgets the speaker. Asking doesn't use it up, so a speaker that vanishes before its prompt is answered gets its prompt again within the 45 s window; once a code was entered, a later refusal gets a notice.
6. S23: a non-finite band gain or preamp counts as 0 dB; any finite value is applied as given.
7. S76, S124: a non-finite volume input is ignored, like a non-finite delay; `gain_for_position` treats NaN as 0 (silence). Negative zero counts as 0, so no volume function returns −0.
8. S64: `GainStage` ramps linearly across the first callback after a gain change (the spec allows a ramp of one callback); its first callback plays at the target. Gain 0 gives positive zeros; negative or non-finite gains count as 0.
9. S26: a block holds `len / 2` frames; an odd trailing sample is left untouched.
10. S31: if a flip races the capture thread's copy, the processor keeps the previous coefficients for one more block. It never waits and never reads a torn set.
11. S152: names compare ignoring case (Unicode lowercase) with ASCII digit runs compared by value. This is a deterministic stand-in for the host's localized comparison.
12. S150: the default group name's text ("Group n") comes from the host as a formatter, so it stays localized.
13. S100: an empty computer name counts as unknown and hides nothing.
14. S157: rates are compared in Hz as `f64`; a reading that is not finite or not above 0 counts as unreadable and keeps the session playing.
15. S161: "sorted by UID" is the byte order of the UID strings; a UID the host lists twice counts once.
16. S162: `session::streaming_after` decides from the numbers of enabled local and AirPlay speakers after the event. A device that leaves or rejoins the lists is an event of its own, and it starts and stops nothing while anything is enabled.
17. S163: two sources are the same when they have the same kind and identifier (bundle ID or device UID). `session::source_pick` answers a pick with `Nothing`, `StoreForNextStart` or `SwitchNow`, listed from the least to the most work; every answer but `Nothing` clears the old source's notices.
18. S164: the quit rule is read as a dependency. `AudioSource::depends_on_app` holds only for an app source and its own bundle ID; the whole system and input devices depend on no app. A quitting app with an empty bundle ID counts as having none.
19. S26, S127: after each block, filter state below 1e-30 (600 dB below full scale) is set to 0. Without it, silence after a bass boost leaves the state decaying into subnormal numbers, where it can hold a rounding limit cycle and every biquad step is slow on x86. With it, the Bass Boost preset's state is all zero about 1.3 s after a loud 40 Hz tone stops; a unit test checks that twenty seconds of silence never end a block on subnormal state.
20. S74: `volume::LatestValues` holds at most one waiting value per target and leaves all timing to the host. `put` reports whether the target had nothing waiting, which is the moment the host queues that target's one flush; any later value rides that flush. The flush takes the latest value with `pop`, and the next value queues a new one.
21. S99: a player status needs `state` to be a JSON string equal to `play`, `pause` or `stop`, case and spacing included. The search expression is percent-encoded like an output ID (S101): everything but unreserved characters. Track ids are integers, as the engine's public JSON API documents them, so the add request needs no further encoding. `PipePlayback` takes the time of the trigger, the session start or the poll, for both kinds of request, so the 15 s are measured alike; "more than 15 s" is strict, and exactly 15 s later a poll doesn't ask. "Any speaker appears" counts every listed speaker, enabled or not, and "the poll selects any speaker" means that poll's reconcile result selects at least one speaker (S104). Step one reads "no AirPlay speaker enabled" as enabled, listed or not; only the poll rule asks for a listed one.

### What stays in the host

Some clauses are covered here only in their pure part:

- S100: decoding the engine's JSON (invalid JSON, a wrong shape or a top-level array give no speakers) stays in the host; `policy::reconcile::speakers` applies the rules to decoded records.
- S99: the HTTP calls, the timers and decoding the JSON; `policy::engine` decides each step of the two-step request, what counts as a player status, the request paths and when a poll asks again.
- S149: the JSON round trip of a group is the host's persistence format.
- S152 and S153: SF Symbol icons and colors, and opening System Settings.
- S7 and S34: `EqProcessor::equalize_and_deliver` fixes the order; the host wires the ring and the FIFO feed to it.
- S167: the Local Network hint for an empty list is the host's (S154).

Clauses only the host can implement are listed with a reason in `tests/spec_exempt.txt` (Core Audio, the OwnTone process, its config file and password, and its HTTP API, macOS privacy and appearance settings, shell tooling). S96 and S97 are there by design: the engine's config file and its per-run password belong to the host that launches the engine. The core never launches the engine or calls its API, and it reaches even the pipe only through the host's `PipeSink`, so taking over just the config text would split one host job across two places.

### Where the two parts meet

- The renderer's output stage maps stereo to a device's channels with a fixed gain (`convert::map_to_device`, S63). `volume::GainStage` and `volume::SharedGain` add the one-callback ramp and the lock-free shared gain; a host can map with gain 1.0 and then run the gain stage, or use `map_to_device` alone.
- Milliseconds to frames (S3) is `reader::delay_ms_to_frames`; `policy::delay` works in milliseconds and leaves the conversion to the renderer.

### Spec coverage checker

`tests/spec_coverage.rs` reads `../docs/ENGINE_SPEC.md`, collects every clause ID and scans `src/` and `tests/` for lines of the exact form `// covers: S44, S49`. It checks that every clause carries an Observed or Required tag and that the IDs are S1 to Sn, each once, in any document order (S1); that every `// covers:` line is well formed and names existing clauses; that the exempt list is well formed; and that every clause is covered or exempt, with no exempt clause covered after all. A clause the core is meant to implement but doesn't yet goes in `tests/spec_pending.txt` with what is missing; the check fails once such a clause gains a `// covers:` test, so the line goes when the core implements it, and a clause can't be both exempt and pending.

## The audio engine through the C interface

This part extends the C interface so a host can run its whole audio engine on the core: the ring and its writer, delay readers, the resampler and an output stage, PCM16 conversion and the FIFO feed, and the AirPlay sync rule. The core still owns no thread, device, file or pipe. Adding functions is compatible, so `ASC_ABI_VERSION` stays 1.

### Handles and threads

- Opaque handles with create and destroy: `AscRing`, `AscRingWriter`, `AscReader`, `AscResampler`, `AscGainStage`, `AscFifoFeed` and `AscFifoWriter`, next to `AscEqualizer`. Plain data crosses as `#[repr(C)]` structs: `AscReadReport`, `AscReaderCounters`, `AscResampleCount`, `AscFifoCounters`, `AscPipeSink`, `AscSyncInput` and `AscSyncDecision`. Codes are `int32_t` values with `#define`s, not C enums, whose size C leaves to the compiler.
- S128, S134: a ring handle, its writer and its readers each hold the ring by `Arc`, and a feed handle and its writer each hold the queue, so they can be destroyed in any order; the memory goes with the last of them, on the thread that destroys it. Destroy calls belong on the control thread once the audio threads stopped.
- S132, S133: a handle that an audio thread shares with other threads keeps the two sides in separate fields (`Reader` and `ReaderControl`, `GainStage` and `SharedGain`, `FifoProducer` and `FifoFeed`). Every call borrows only its own field through the raw pointer, never the whole handle, so a read and a control call can overlap without a `&mut` aliasing anything. The other side's calls are relaxed atomic stores and loads, or the release flag and acquire exchange of a resync.
- S35, S39: `asc_ring_writer_create` returns NULL while a writer exists. "No reader exists" at a reset stays the host's duty, as in Rust: `asc_ring_writer_reset` doesn't check it.
- S41, S46: `asc_reader_read` reads at the target that `asc_reader_set_delay_frames` stores before raising the resync flag; `asc_reader_read_at` is S41's read with the caller's delay and tolerance. Both fill an optional `AscReadReport`, all zero when nothing was read.
- S46, S59: a read of 0 frames reads nothing. It returns the all-zero report and leaves a pending resync or prime reset to the next read. A resampled output can need no input in a cycle, and if an empty read took the resync, its anchor wouldn't advance with the writer: the next read would start late by the frames written in between, inside the dead band, so nothing would correct it and a delay change would no longer be exact. The Rust `Reader` follows S44 as written; the C interface skips the empty call.
- S16, S36: a ring's capacity runs from 16 384 frames (twice the margin) to 24 s at 384 kHz (9 216 000 frames, about 74 MB); `asc_ring_capacity_for_rate` gives the 24 s capacity of a rate. The upper bound keeps a nonsense capacity from overflowing the allocation size.
- S62: `Resampler::input_frames_for`, behind `asc_resampler_input_frames_for`, now computes in 128 bits and saturates; a request near `usize::MAX` used to overflow (a panic in debug builds).
- S62: a resampler's calls run on the thread that owns it, which is the output thread once the output runs, so every call but create, destroy and the rate change is real-time safe: the output thread sizes each reader request with `asc_resampler_input_frames_for`, and it may start a new stream with `asc_resampler_reset` or read `asc_resampler_latency_frames`. The rate change never allocates but recomputes the filter bank, so it isn't marked: a capture rate change restarts the session (S140), and the host changes the rate while the output is stopped.
- S85, S125: with different rates, a non-finite input sample becomes silence and clears the resampler's history. Equal rates pass every sample through unchanged, NaN and infinity included, as S85 asks; with no history there is nothing for a NaN to stay in. Nothing non-finite reaches a device or the pipe: the output stage zeroes it (S65) and PCM16 conversion maps it (S86).
- S63 to S65: `asc_gain_stage_render` maps stereo onto the device's channels at gain 1, then runs the gain stage, which plays the first buffer at the gain set, ramps across one buffer after a change and zeroes non-finite values. `asc_gain_stage_apply` runs the stage alone on a buffer the host already mapped.

### Real-time rules at the boundary

- A real-time call takes a pointer and a count, returns at once for a NULL handle, a NULL buffer with a nonzero count or a count whose byte size can't exist (above `isize::MAX`), and otherwise calls the Rust code, which never allocates, locks or panics. A NULL buffer with a count of 0 is an empty buffer. Absurd values saturate: a delay beyond the ring is capped (S42), a huge resampler request saturates, a write count above what was offered is clamped.
- Nothing unwinds across the boundary: release builds abort on panic, an `extern "C"` function aborts rather than unwind, and the tests drive every function with NULL handles, NULL buffers, zero counts and `usize::MAX`.

### FIFO writer

- S88, S129: the host owns the writer thread and the pipe. At each start it creates an `AscFifoWriter`, which takes the queue's only consumer (a second writer is refused) and empties the queue, then calls `asc_fifo_writer_step` in a loop, sleeping the microseconds each step returns: 0 to step again, 2 000 on a full pipe, 4 000 with nothing queued, 50 000 while the pipe has no reader. To stop, it wakes the thread, calls `asc_fifo_writer_close_step` until it returns false, destroys the writer and joins the thread.
- The pipe is an `AscPipeSink`: a context pointer and three C callbacks, `open` (`open(2)` write-only with `O_NONBLOCK`, then `F_SETNOSIGPIPE`), `write` (`write(2)`) and `close` (`close(2)`). They report `ASC_PIPE_*` codes for success, `ENXIO`, `EINTR`, `EAGAIN` and `EPIPE`; any other code counts as a failure, and a NULL callback fails. They run only inside a step, on the host's writer thread, so the frame alignment, the 512-byte writes, the backlog cap, resuming a short write, reconnecting on a frame boundary and starting each connection at the live edge (S89 to S93, S169) are the core's `FifoWriter`, unchanged. A host that feeds before the first step that opens the pipe sees that audio dropped, like anything else queued at an open.
- S88, S93: `asc_fifo_writer_close_step` completes a partly written frame, then closes the pipe. Each attempt is charged at least the 2 ms full-pipe pause against a 100 ms budget, so an interrupted callback can't loop forever and a stop stays well under 300 ms; after that the rest of the frame is given up, as `FifoControl::stop` does.
- S84, S87, S160: `asc_fifo_counters` returns the writer's counters plus `samples_queued` and `frames_dropped` in one struct.

### Sync rule

- S78 to S80: `asc_sync_decide` takes the enabled AirPlay IDs, the engine's listed IDs and the previous decision, and returns whether an AirPlay speaker is chosen and whether that flipped; `asc_sync_effective_delay_ms` gives each output its delay. IDs are UTF-8 C strings; a NULL entry or one that isn't UTF-8 matches nothing. The rule is `policy::delay::airplay_chosen` itself, called with two vectors of IDs, so the call allocates: it belongs on the control thread. `asc_delay_sanitize_ms` (S77, S124) and `asc_delay_ms_to_frames` (S3) are the matching helpers.

### AirPlay sender (first stage: discovery)

- `src/ffi/airplay.rs` is written from the airplay crate's public API and `docs/AIRPLAY_SPEC.md`, not from any AirPlay implementation. `asc_airplay_create` starts a `Sender` (its stream stays silent: nothing writes its input yet), `asc_airplay_start_discovery` and `asc_airplay_stop_discovery` browse, and `asc_airplay_next_receiver` hands over one discovery change at a time without waiting: the receiver's ID (the Windows app's form: 12 hex digits, or `raop:` / `airplay:` and the instance), name, model, availability (the Windows app's order: the plan, then the PIN pairing, a password, another pairing), the unsupported reason and the discovery hints as flag bits. The sender's other events are passed over until audio comes in a later stage. No password, PIN, key or credential crosses.
- Strings belong to the handle and last until the next `asc_airplay_next_receiver` call or the destroy, so no free function is needed. `asc_airplay_destroy` takes the address of the host's pointer and sets it to NULL, which makes a second destroy through it harmless; it waits for the sender's threads (at most about 2 s), so it belongs off the main thread. Every call on a handle runs on one control thread at a time; none is real-time safe.

### Checks

- The drift test in `src/ffi.rs` reads every file of the `ffi` module and fails when a file is missing from its list. It compares the prototypes, the `#[repr(C)]` structs field by field, the opaque handles, the callback types and every `#define` with the Rust definitions, and every `Asc` type the header names must be declared in it. It also checks that the header and the Rust docs mark the same functions "Real-time safe", and that the allocation proof calls each of them, since a host reads only the header to learn what its audio threads may call.
- The counting allocator in `src/ffi.rs` proves that every real-time call of the C interface neither allocates nor frees: the ring write, both reads, the reader's control calls and counters, the resampler (its process, request sizing, reset and latency), the gain stage, PCM16 conversion, the FIFO feed and its rate change, and writer steps and close steps through a pipe sink that refuses opens, fills up, cuts writes short, gets interrupted and breaks. It does so on one thread, and on five threads at once (capture, two outputs, the FIFO writer and a control thread), each counted on its own.
- `tests/realtime.rs` scans the sources for locks, sleeps, logging and file access: the audio-path modules, which also have to forbid unsafe code, and the production part of `ffi.rs`, `ffi/ring.rs`, `ffi/render.rs` and `ffi/fifo.rs`, which hold the crate's unsafe code and so skip only that check. A lock or a log line added to a C call is caught even when it doesn't allocate.
- `ffi/smoke.sh` checks from Swift: a ring with a reader at 4096, 8192 and 2048 frames of delay (silent and not anchored until more than 4096 frames are written, then every block starts exactly the delay behind the writer, a longer delay inserts exactly its extra frames of silence, a shorter one skips ahead on the next block, no frame plays twice), PCM16 conversion, the FIFO writer stepped through Swift `@convention(c)` callbacks that record the bytes, 2 s at 44.1 kHz becoming 96 000 frames at 48 kHz, and the sync rule over Swift strings passed as a C array.
- `scripts/build_core.sh` still prelinks the library and exports exactly the header's functions: 71 global symbols on each architecture.
