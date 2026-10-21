# audiosync-core implementation notes

Everything in this crate is written from `docs/ENGINE_SPEC.md`, whose clause IDs (`S1` and up) appear in comments and test names, and from public documentation (Rust and Swift docs, the RBJ Audio EQ Cookbook, the cargo-deny docs). No other source in this repository was used.

Where the spec leaves room, the readings below were chosen; each one satisfies every Observed clause. The crate was built in three parts, and these notes keep one section for each.

## Audio path: ring, delay reader, resampler and conversion

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
- **S125**: a frame with a non-finite sample is stored as silence and zeroes all earlier history. A sum that overflows to a non-finite value on huge finite input is written as 0. When the rates are equal, samples pass through bit-exact (S85).

### Conversion (`convert.rs`)

- **S63-S65**: `map_to_device` is offered for the renderer's output stage. It handles channel mapping, gain, and replaces anything non-finite with 0 after the gain.

### FIFO feed (removed)

The core's PCM pipe feed (`fifo.rs`, S82 to S95, S160, S169, S203) went on 2026-10-20 with the bundled AirPlay engine it fed, and with it the float to PCM16 conversion in `convert.rs` (S83, S86); its readings are in this file's git history.

### Build

- `[profile.test] opt-level = 1`: the two-hour drift simulations then run in about a second. Debug assertions and overflow checks stay on.

## Equalizer, volume, host policies, C interface and CI

### Modules

- `eq`: the ten-band equalizer and preamp (S22 to S34), split into `EqControl` (main thread) and `EqProcessor` (capture thread) that share only atomics.
- `volume`: the cubic volume curve, host-side volume rules and the renderer's gain stage (S64, S65, S70, S71, S75, S76). The bundled engine's volume helpers (`engine_volume`, `adopt_reported_volume`, `LatestValues`, S72 to S74) went with it on 2026-10-20.
- `channels`: an input device's channels to stereo (S14).
- `fade` and `render`: the 20 ms fades at stream boundaries and a local output's renderer (S182 to S191, see the last part).
- `policy`: pure host decisions the Windows app can reuse: delays and the AirPlay sync rule; the capture source, the microphone permission and app quits; the capture check and capture rate changes; sessions, output sync, streaming and source switches; speaker keys, groups, device lists, layout and notices.
- `ffi`: the C interface, `ffi.rs` plus `ffi/ring.rs`, `ffi/render.rs`, `ffi/sync.rs`, `ffi/device_volume.rs` and `ffi/airplay.rs` (see the last part).

### C interface

Decision: a plain C ABI (`extern "C"` with `#[unsafe(no_mangle)]`) declared by the hand-written header `include/audiosync_core.h`. No UniFFI and no cbindgen.

Why:

- Real time. Audio crosses the boundary a whole buffer at a time, as a pointer plus a frame count, and is processed in place: no copy and no allocation. UniFFI passes buffers through its own serialized `RustBuffer`, which allocates and copies on every call, so it can't sit on a Core Audio callback.
- No runtime or build dependency. UniFFI needs its crate, generated bindings and a build step; cbindgen needs a tool in CI. The surface was 16 functions at first and is 108 now.
- Swift imports a C header directly through a module map, and a Windows host can use the same header or link the `rlib`.
- The header can't drift silently: a unit test in `src/ffi.rs` parses the header and the Rust definitions, maps the types, and fails when any prototype, struct, handle, callback type or `#define` differs. CI runs it with the other tests, so "the header is current" is checked without a generator.

Contract (also in the header): `asc_eq_process` runs on one capture thread at a time; the other `asc_eq_*` calls on a handle run on one control thread at a time; the two touch different fields of the handle. Null pointers make a call do nothing. All `unsafe` code of the crate is in the `ffi` module (`src/ffi.rs` and `src/ffi/`) and in `src/panic_probe.rs`, which only the CI probe build has (feature `ffi-panic-probe`), every block with a SAFETY comment (`clippy::undocumented_unsafe_blocks` is on in both); the other modules `#![forbid(unsafe_code)]`.

`ffi/smoke.sh` builds the static library and a small Swift program (`ffi/swift-smoke/`, module map plus `main.swift`) that calls the version, the volume curve, the presets, one equalizer pass, a ring with a reader, the delay helpers, the local delay model and the AirPlay sender's handle through the header. CI runs it on macOS. It touches no audio device and no network.

### Real-time checks

- A counting global allocator in the test module of `src/ffi.rs` (there so that every `unsafe` line stays in the `ffi` module) proves that the equalizer (including picking up new settings while the control thread keeps publishing, and the non-finite reset path), `equalize_and_deliver`, the C process call, `GainStage`, `apply_gain` and `input_to_stereo` neither allocate nor free. `tests/realtime.rs` does the same for the ring, the reader, the resampler, the conversion, the renderer and the fades.
- `tests/realtime.rs` also scans the production code of every audio-path module for locks, blocking waits, logging and file access. It reads the sources at run time and normalizes line ends, so it works on any checkout.
- The equalizer shares two coefficient sets of `AtomicU64` values plus SeqCst `active` and `reading` indices; there is no lock anywhere. Building fails on a target without native 64-bit atomics (S128).
- A unit test counts biquad steps: exactly 20 per frame (S127).
- `benches/eq.rs` (criterion): about 7.2 µs per 512-frame stereo block with all ten bands active on an Apple silicon Mac, under 0.1 % of the block's 10.7 ms at 48 kHz.

### Dependencies and licenses

- Runtime dependency: `audiosync-airplay` (`airplay/` in this repository, GPL-3.0-or-later like the core), AudioSync's own AirPlay sender, which the macOS app reaches through this crate's C interface because it links one Rust static library; its third-party crates are listed in `THIRD_PARTY_NOTICES.md`, and `scripts/core_notices.py` writes their license texts next to the library. It raised the minimum Rust from 1.85 to 1.88, the sender's; the two clippy lints that turns on for newer std APIs (`as_chunks`, `is_multiple_of`) are allowed in `Cargo.toml` for the existing audio code.
- Dev-dependencies: `criterion` 0.7 (benchmarks) and `proptest` 1.11 (property tests). Both are MIT OR Apache-2.0.
- `deny.toml` allows only MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, Zlib, ISC and Unicode-3.0 (Unicode-DFS-2016 isn't needed). `include-dev` and `include-build` hold test, benchmark and build tools to the same list; without them cargo-deny checks only the crate itself. CI installs cargo-deny 0.20.2, the version checked locally.

### Spec readings

Where the spec left room, these readings satisfy every Observed clause:

1. S106 access kinds (a record: `policy::pairing` went with the bundled engine on 2026-10-20): a PIN flag (`needs_auth_key`) needs a code; so does a password on an AirPlay 2 speaker. The code is asked as a password when the speaker has one. An AirPlay 1 speaker with a password and no PIN flag is a legacy password speaker. This satisfies both "availability: password asks for a code" and "availability: AirPlay 1 password unsupported".
2. S106 to S108 clicks (a record, as 1): the user's click is kept until a code answers it, or a cancel, turn-off or automatic turn-on forgets the speaker. Asking doesn't use it up, so a speaker that vanishes before its prompt is answered gets its prompt again within the 45 s window; once a code was entered, a later refusal gets a notice.
3. S23: a non-finite band gain or preamp counts as 0 dB; any finite value is applied as given.
4. S76, S124: a non-finite volume input is ignored, like a non-finite delay; `gain_for_position` treats NaN as 0 (silence). Negative zero counts as 0, so no volume function returns −0.
5. S64: `GainStage` ramps linearly across the first callback after a gain change (the spec allows a ramp of one callback); its first callback plays at the target. Gain 0 gives positive zeros; negative or non-finite gains count as 0.
6. S26: a block holds `len / 2` frames; an odd trailing sample is left untouched.
7. S31: if a flip races the capture thread's copy, the processor keeps the previous coefficients for one more block. It never waits and never reads a torn set.
8. S152: names compare ignoring case (Unicode lowercase) with ASCII digit runs compared by value. This is a deterministic stand-in for the host's localized comparison.
9. S150: the default group name's text ("Group n") comes from the host as a formatter, so it stays localized.
10. S157: rates are compared in Hz as `f64`; a reading that is not finite or not above 0 counts as unreadable and keeps the session playing.
11. S161: "sorted by UID" is the byte order of the UID strings; a UID the host lists twice counts once.
12. S162: `session::streaming_after` decides from the numbers of enabled local and AirPlay speakers after the event. A device that leaves or rejoins the lists is an event of its own, and it starts and stops nothing while anything is enabled.
13. S163: two sources are the same when they have the same kind and identifier (bundle ID or device UID). `session::source_pick` answers a pick with `Nothing`, `StoreForNextStart` or `SwitchNow`, listed from the least to the most work; every answer but `Nothing` clears the old source's notices.
14. S164: the quit rule is read as a dependency. `AudioSource::depends_on_app` holds only for an app source and its own bundle ID; the whole system and input devices depend on no app. A quitting app with an empty bundle ID counts as having none.
15. S26, S127: after each block, filter state below 1e-30 (600 dB below full scale) is set to 0. Without it, silence after a bass boost leaves the state decaying into subnormal numbers, where it can hold a rounding limit cycle and every biquad step is slow on x86. With it, the Bass Boost preset's state is all zero about 1.3 s after a loud 40 Hz tone stops; a unit test checks that twenty seconds of silence never end a block on subnormal state.
16. S74 (a record: `volume::LatestValues` went with the bundled engine on 2026-10-20): `volume::LatestValues` holds at most one waiting value per target and leaves all timing to the host. `put` reports whether the target had nothing waiting, which is the moment the host queues that target's one flush; any later value rides that flush. The flush takes the latest value with `pop`, and the next value queues a new one.

The readings of the bundled engine's clauses (S98 to S100, S102, S103, S168) went with the policy modules that followed them (`policy::engine`, `policy::reconcile`, `policy::track_info`) on 2026-10-20.

### What stays in the host

Some clauses are covered here only in their pure part:

- S149: the JSON round trip of a group is the host's persistence format.
- S152 and S153: SF Symbol icons and colors, and opening System Settings.
- S7 and S34: `EqProcessor::equalize_and_deliver` fixes the order; the host wires the ring and the AirPlay sender's input to it.
- S167: the Local Network hint for an empty list is the host's (S154).

Clauses only the host can implement are listed with a reason in `tests/spec_exempt.txt` (Core Audio, process signals, macOS privacy and appearance settings, shell tooling, and the clauses of the bundled engine AudioSync used before, retired on 2026-10-20 with it: its process, its config file and password, and its HTTP API; later that day the FIFO feed's clauses and those of the engine's policy modules joined them, when that code went too). S96 and S97 were there by design: the engine's config file and its per-run password belonged to the host that launched the engine. The core never launched the engine or called its API, and it reached even the pipe only through the host's pipe sink, so taking over just the config text would have split one host job across two places.

### Where the two parts meet

- The renderer's output stage maps stereo to a device's channels with a fixed gain (`convert::map_to_device`, S63). `volume::GainStage` and `volume::SharedGain` add the one-callback ramp and the lock-free shared gain; a host can map with gain 1.0 and then run the gain stage, or use `map_to_device` alone.
- Milliseconds to frames (S3) is `reader::delay_ms_to_frames`; `policy::delay` works in milliseconds and leaves the conversion to the renderer.

### Spec coverage checker

`tests/spec_coverage.rs` reads `../docs/ENGINE_SPEC.md`, collects every clause ID and scans `src/` and `tests/` for lines of the exact form `// covers: S44, S49`. It checks that every clause carries an Observed or Required tag and that the IDs are S1 to Sn, each once, in any document order (S1); that every `// covers:` line is well formed and names existing clauses; that the exempt list is well formed; and that every clause is covered or exempt, with no exempt clause covered after all. A clause the core is meant to implement but doesn't yet goes in `tests/spec_pending.txt` with what is missing; the check fails once such a clause gains a `// covers:` test, so the line goes when the core implements it, and a clause can't be both exempt and pending.

## The audio engine through the C interface

This part extends the C interface so a host can run its whole audio engine on the core: the ring and its writer, delay readers, the resampler and an output stage, and the delays of the local delay model (the PCM16 conversion, the FIFO feed and the old AirPlay sync rule's calls it carried at first went with the bundled engine on 2026-10-20). The core still owns no thread, device or file. Adding functions is compatible, so `ASC_ABI_VERSION` stays 1.

### Handles and threads

- Opaque handles with create and destroy: `AscRing`, `AscRingWriter`, `AscReader`, `AscResampler`, `AscGainStage`, next to `AscEqualizer`. Plain data crosses as `#[repr(C)]` structs: `AscReadReport`, `AscReaderCounters` and `AscResampleCount`, among others. Codes are `int32_t` values with `#define`s, not C enums, whose size C leaves to the compiler.
- S128, S134: a ring handle, its writer and its readers each hold the ring by `Arc`, so they can be destroyed in any order; the memory goes with the last of them, on the thread that destroys it. Destroy calls belong on the control thread once the audio threads stopped.
- S132, S133: a handle that an audio thread shares with other threads keeps the two sides in separate fields (`Reader` and `ReaderControl`, `GainStage` and `SharedGain`). Every call borrows only its own field through the raw pointer, never the whole handle, so a read and a control call can overlap without a `&mut` aliasing anything. The other side's calls are relaxed atomic stores and loads, or the release flag and acquire exchange of a resync.
- S35, S39: `asc_ring_writer_create` returns NULL while a writer exists. "No reader exists" at a reset stays the host's duty, as in Rust: `asc_ring_writer_reset` doesn't check it.
- S41, S46: `asc_reader_read` reads at the target that `asc_reader_set_delay_frames` stores before raising the resync flag; `asc_reader_read_at` is S41's read with the caller's delay and tolerance. Both fill an optional `AscReadReport`, all zero when nothing was read.
- S46, S59: a read of 0 frames reads nothing. It returns the all-zero report and leaves a pending resync or prime reset to the next read. A resampled output can need no input in a cycle, and if an empty read took the resync, its anchor wouldn't advance with the writer: the next read would start late by the frames written in between, inside the dead band, so nothing would correct it and a delay change would no longer be exact. The Rust `Reader` follows S44 as written; the C interface skips the empty call.
- S16, S36: a ring's capacity runs from 16 384 frames (twice the margin) to 24 s at 384 kHz (9 216 000 frames, about 74 MB); `asc_ring_capacity_for_rate` gives the 24 s capacity of a rate. The upper bound keeps a nonsense capacity from overflowing the allocation size.
- S62: `Resampler::input_frames_for`, behind `asc_resampler_input_frames_for`, now computes in 128 bits and saturates; a request near `usize::MAX` used to overflow (a panic in debug builds).
- S62: a resampler's calls run on the thread that owns it, which is the output thread once the output runs, so every call but create, destroy and the rate change is real-time safe: the output thread sizes each reader request with `asc_resampler_input_frames_for`, and it may start a new stream with `asc_resampler_reset` or read `asc_resampler_latency_frames`. The rate change never allocates but recomputes the filter bank, so it isn't marked: a capture rate change restarts the session (S140), and the host changes the rate while the output is stopped.
- S85, S125: with different rates, a non-finite input sample becomes silence and clears the resampler's history. Equal rates pass every sample through unchanged, NaN and infinity included, as S85 asks; with no history there is nothing for a NaN to stay in. Nothing non-finite reaches a device: the output stage zeroes it (S65).
- S63 to S65: `asc_gain_stage_render` maps stereo onto the device's channels at gain 1, then runs the gain stage, which plays the first buffer at the gain set, ramps across one buffer after a change and zeroes non-finite values. `asc_gain_stage_apply` runs the stage alone on a buffer the host already mapped.
- S62 to S65, S183 to S188: `AscRenderer` is a whole local output's renderer, `render::Renderer` with its fades, for one start on one device: `asc_renderer_create` makes its own reader on the ring, a `RendererControl` and the shared gain, and fixes the device rate and the channels, so a host makes a new one at each start. Its two sides are separate fields: the output thread's `asc_renderer_render` and `asc_renderer_render_timed` borrow the `Renderer`, every other call the control, the gain or F, and those are atomics. The timed render takes the callback's output time in host nanoseconds, so a stop's fade-out publishes when its last frame is output (`asc_renderer_fade_end_time_ns`, `asc_renderer_faded_by_time`, S185 on the Mac). `asc_renderer_fade_state` reads where the fade was at the end of the latest callback, which the renderer now publishes with a relaxed store (`RendererControl::fade_state`), for diagnostics and the Mac's hardware checks. `ASC_STOP_FADE_CAP_MS` is `render::STOP_FADE_CAP`.

### Real-time rules at the boundary

- A real-time call takes a pointer and a count, returns at once for a NULL handle, a NULL buffer with a nonzero count or a count whose byte size can't exist (above `isize::MAX`), and otherwise calls the Rust code, which never allocates, locks or panics. A NULL buffer with a count of 0 is an empty buffer. Absurd values saturate: a delay beyond the ring is capped (S42), a huge resampler request saturates, a write count above what was offered is clamped.
- Nothing unwinds across the boundary: an `extern "C"` function aborts rather than unwind, and the tests drive every function with NULL handles, NULL buffers, zero counts and `usize::MAX`. Release builds unwind since the AirPlay sender (see "Panics" below), so a panic in an audio-path call still ends the app, as it did when they aborted.

### Sync rule

- S78, the old rule: its C calls went on 2026-10-20, since no host called them. `asc_sync_effective_delay_ms` gave each output its delay under that rule, which the local delay model below replaced (`policy::delay::effective_delay_ms` still states it in Rust, checked by `tests/policy_delay.rs`); `asc_sync_decide` decided whether an AirPlay speaker is chosen from the bundled engine's listed IDs, which the Mac's own sender now decides itself. `asc_delay_sanitize_ms` (S77, S124) and `asc_delay_ms_to_frames` (S3) are the delay helpers that stay.
- S172 to S179, the local delay model that replaces that rule: `asc_local_hold_backs_ms` turns the offsets of the outputs taking part (the reference marked, its offset counting as 0) and L while an AirPlay speaker is chosen into each renderer's hold-back (`policy::delay::hold_backs`); the host re-times a running renderer only when its hold-back changed (`policy::delay::Retimer` does the same bookkeeping in Rust). `asc_local_offset_sanitize_ms`, `asc_local_airplay_delay_sanitize_ms`, `asc_local_native_latency_ms` and `asc_local_native_adjustment_ms` are the settings' helpers (S175, S178, S181), and `asc_local_delay_migrate` is the Mac's one-time migration of its stored absolute delays as a pure function (S179); reading and writing the defaults keys stays in the host. The reference choice (S173, S174) and the windows (S181) stay in the hosts.

### AirPlay sender (discovery, speakers, events and the capture's input)

- `src/ffi/airplay.rs` is written from the airplay crate's public API, `docs/AIRPLAY_SPEC.md` and the Windows app's own use of the crate, not from any AirPlay implementation. `asc_airplay_create` starts a `Sender`, `asc_airplay_start_discovery` and `asc_airplay_stop_discovery` browse, and `asc_airplay_next_receiver` hands over one discovery change at a time without waiting: the receiver's ID (the Windows app's form: 12 hex digits, or `raop:` / `airplay:` and the instance), name, model, availability (the Windows app's order: the plan, then the PIN pairing, a password, another pairing), the unsupported reason and the discovery hints as flag bits. No password, PIN, key or credential crosses; PIN pairing comes in a later stage.
- Speakers are added and removed by receiver ID (`asc_airplay_add_speaker`, `asc_airplay_remove_speaker`): the handle keeps the devices discovery lists, so the host never sees a device record. Volumes cross as slider positions, the speaker's and the master's (`asc_airplay_set_volume`, `asc_airplay_set_master`), and the handle maps their product onto −30 to 0 dB, muted at 0, as the Windows app does (P118 leaves the mapping to the host). Every speaker gets d_s = 0 for now.
- `asc_airplay_next_event` hands over the sender's other events, one at a time: a speaker's state (connecting, playing, retrying with the time until the next try, failed, stopped), an error with what to tell the user (the crate's `UserReason`, the unsupported reason, the retry time or -1 when the user must act, and the error text for a log), a speaker's latency (L + d_s and the receiver's own `Audio-Latency`), and the timeline with L (P98, P113, P143). Warnings, methods, server and jack reports, volume reads and pairing events are passed over. Receiver changes and these events wait in two queues of the handle, so taking one kind never drops the other. `asc_airplay_latency_ms` reads L and `asc_airplay_restart_timeline` tells the sender the capture restarted (P143).
- The capture's audio goes through an input handle that `asc_airplay_take_input` hands over once. `asc_airplay_input_write` resamples a block from the capture rate to 44.1 kHz with the core's `Resampler` and hands it to the sender's `InputWriter` stamped with the time it arrived (P73, P96); it never allocates, locks, waits or logs (P6, S126) and reads the monotonic clock once. `asc_airplay_input_set_rate` is an any-thread call that only bumps an atomic request (a count and the rate); the next write takes it in and starts the resampler afresh, so a rate request never races a write. The input holds its own reference to the sender's ring, so it may outlive the sender. The write returns the frames handed over; the sender drops late ones itself (P112).
- Panics: the release profile unwinds so that a bug in the sender becomes a failed sender (`ASC_AIRPLAY_EVENT_FAILED`, which the host shows with Retry) instead of ending the app. Every `asc_airplay_*` call runs under `catch_unwind` and marks its handle dead on a panic; a driver thread that ended counts as dead too, and so does discovery whose mDNS daemon ended (the browser's channel is disconnected, the sender reports `DiscoveryFailed`). The sender's connect, RTSP, timing and control threads catch their own panics and report a failed connect or a closed connection, so the speaker is retried at once. Everything else keeps the old behavior and ends the app: any other `extern "C"` function aborts on a panic, and a thread the core spawns itself would abort the process on one; `Cargo.toml` states the rule for new threads. A failed sender's detail says where the panic happened, as a file and line with the path cut to the crate, never the panic's message (S205).
- Strings belong to the handle and last until the next call of the same kind (`asc_airplay_next_receiver` or `asc_airplay_next_event`) or the destroy, so no free function is needed. `asc_airplay_destroy` and `asc_airplay_input_destroy` take the address of the host's pointer and set it to NULL, which makes a second destroy through it harmless; the sender's destroy waits for its threads (at most about 2 s), so it belongs off the main thread. Every call on a sender handle runs on one control thread at a time; the input's write and rate request are the real-time safe ones.

### Checks

- The drift test in `src/ffi.rs` reads every file of the `ffi` module and fails when a file is missing from its list. It compares the prototypes, the `#[repr(C)]` structs field by field, the opaque handles, the callback types and every `#define` with the Rust definitions, and every `Asc` type the header names must be declared in it. It also checks that the header and the Rust docs mark the same functions "Real-time safe", and that the allocation proof calls each of them, since a host reads only the header to learn what its audio threads may call.
- The counting allocator in `src/ffi.rs` proves that every real-time call of the C interface neither allocates nor frees: the ring write, both reads, the reader's control calls and counters, the resampler (its process, request sizing, reset and latency), the gain stage, and the renderer's renders with its control calls. It does so on one thread, and on four threads at once (capture, two outputs and a control thread), each counted on its own.
- `tests/realtime.rs` scans the sources for locks, sleeps, logging and file access: the audio-path modules, which also have to forbid unsafe code, and the production part of `ffi.rs`, `ffi/ring.rs` and `ffi/render.rs`, which hold the crate's unsafe code and so skip only that check. A lock or a log line added to a C call is caught even when it doesn't allocate.
- `ffi/smoke.sh` checks from Swift: a ring with a reader at 4096, 8192 and 2048 frames of delay (silent and not anchored until more than 4096 frames are written, then every block starts exactly the delay behind the writer, a longer delay inserts exactly its extra frames of silence, a shorter one skips ahead on the next block, no frame plays twice), the delay helpers, the local delay model and the AirPlay sender's handle.
- `scripts/build_core.sh` still prelinks the library and exports exactly the header's functions: 108 global symbols on each architecture (109 in the CI probe build, `AUDIOSYNC_CORE_PROBE=1`, which adds `asc_airplay_panic_probe`).
- The AirPlay input's write and rate request are in the allocation proof; `tests/realtime.rs` doesn't scan `ffi/airplay.rs`, whose control calls talk to the sender's threads.
- Panic tests run in `cargo test` builds only. The C interface's panics come from `injected_panic`, which exists only under `cfg(test)`; no test runs a panic through the shipped, prelinked Mac library, so that the release profile really unwinds there and the app shows a failed sender instead of crashing is a check on the Mac.

## Fades at stream boundaries

This part implements S182 to S185 and S187 to S191: a 20 ms fade at every start, stop, source change and re-timing of a local output, and on the AirPlay leg's inputs. S186 (the order of a session stop and its bounded wait) stays with the hosts; the core gives them the fade requests, what tells them a fade has played, and `render::STOP_FADE_CAP`.

### Fade (`fade.rs`)

- S182: w is computed per frame in f64 (`cos`), so nothing needs a table and nothing allocates after construction; the last frame of a fade-in is exactly 1 and that of a fade-out exactly 0. F is `round(0.020 × R)` in integers, at least 1.
- S182, reversal: a fade-in that has rendered p frames turns into a fade-out whose next frame is F − p on the fade-out curve (its frame F − p − 1 has the same w), and the other way round, so the way back takes exactly as many frames as the way there. A fade-in that rendered nothing turns straight into closed, a fade-out that rendered nothing into open.
- `Fade::apply` leaves open frames untouched (bit-exact), writes exact zeros while closed, and replaces NaN and ±infinity by 0 before multiplying while fading. It reports the frame that ended a fade-out, so the renderer knows where a stop's fade ended.
- `Fade::set_rate` keeps a fade under way at the same position on its curve, in the new length's frames.
- `InputFade` is the native AirPlay input's fade (S189), used by both hosts' inputs: closed until `restart(rate)`, which fades in; `fade_out` fades out and then gives zeros. While open it hands the caller's block on as it is; otherwise it hands faded copies in pieces of a buffer allocated at construction, so the ring keeps its unchanged input (S7).

### Renderer (`render.rs`)

- The renderer is the one the hosts composed from the reader, the resampler, `map_to_device` and the gain stage (S62 to S65), with the fade after them. It works in pieces of at most `RENDER_CHUNK` (512) device frames, one read each. `GainStage::begin` returns the callback's `GainRamp`, which the renderer applies piece by piece, so the gain still ramps across the whole callback (S64) and the fade multiplies after it (S183); `GainStage::apply` is the same ramp over a whole buffer.
- S184: a read reports its leading silence (`ReadReport::leading_silence`: the masked frames, or the whole block before its trailing silence when the post-copy check discarded the copy) and its copies. Without conversion the piece's frames are the read's frames; with conversion the renderer maps the read's frame positions proportionally onto the piece (`lead × n / need`), ignoring the converter's delay, so a fade-in may start the converter's few frames early and the silence after a starved read closes the output that many frames early (the start of an underrun is an unfaded edge anyway).
- S185: the control's `request_fade_out` is taken at the start of the next callback. When the fade-out ends, the renderer publishes the output time of its last frame (the callback's output time plus that frame's offset, `offset × 10⁹ / rate` in integer nanoseconds) if the host gave one (`render_timed`), and counts the device frames it renders after that frame (`frames_after_fade`). A stop asked for while nothing is audible ends at once; its time is then the callback's own output time, and every frame of that callback counts as after it. `faded_by_time` is the Mac's rule (the host clock against that time plus the device's and stream's latency); `endpoint_faded` is the Windows rule in frames: faded once the frames written after the fade cover the endpoint's padding plus the stream latency, which means only zeros are queued and at least the latency's worth of frames has played since the last faded one left the queue.
- S187: `RendererControl::set_delay_frames` stores the value before its flag. A renderer that is closed hands the delay to its reader at once; otherwise it keeps it until the fade-out's last frame (a newer one replaces it, the fade isn't restarted). Either way the renderer then stays closed until a read reports `Resync` or `Prime`, which is the read that applies the delay, so no frame is ever copied on the old timeline after the fade-out.
- S188: the renderer reads with corrections held (`Reader::read_target_held`) whenever its fade isn't closed. A held skip forward or step back is reported in `ReadReport::due`, counts nothing and starts the fade-out at the first frame of that read's piece; the first read after the fade-out is unheld because the renderer is then closed, and that read ends the wait whether or not a correction is still due. Overruns are not held (S188 leaves them unfaded).
- A fade-out that a frame of reader silence cuts short ends there as if its last frame had been rendered: a waiting delay goes to the reader, and a stop's end is that frame.
- S209: a callback's lateness is `now - output time` above the second lowest value seen, so one stray early stamp never counts. The lowest two leak up by 1 ms per second of device frames and stay within 50 ms of the current value; when two 1 s windows in a row each lie wholly more than 5 ms above them, they rise to the lower of those windows' lowest values, so a burst of early stamps stops counting within three windows while 1.5 s of late callbacks lifts nothing. `floor_lifts` counts the lifts. The lateness is capped at 50 ms. It goes to the reader as Jr, added to De after the floors, and S207's fill takes the corrected time. Only `render_timed_at` has both times, so the Windows renderer and every untimed call keep Jr at 0.
- `RendererControl::new(&reader)` is made before the reader moves to its output thread, so the host can keep the control; `Renderer::new` refuses a control made for another reader.
- `endpoint_faded` and the published time are plain arithmetic on the audio thread; the requests and the published values are atomics (release stores, acquire loads), so a renderer never locks or waits (S190). `tests/realtime.rs` scans `fade.rs` and `render.rs` like the other audio-path modules and counts no allocation across renders with delay changes, held corrections, a stop and the native input's fades.

### The AirPlay leg (S189)

- The C interface's native input fades in after each `asc_airplay_input_set_rate` and out after `asc_airplay_input_request_fade_out`, at the capture rate, before its resampler to 44.1 kHz. The Windows app's AirPlay feed does the same through the same `InputFade`, fading in at each lease and out after `FeedSlot::request_fade_out`.

### Quit budget

`policy::session::QUIT_BUDGET` (5 s) is the most a quit waits in all, so a host can check that `STOP_FADE_CAP` (0.5 s) and its own exit waits fit in it; the Windows app's test adds the AirPlay sender's teardown and the debug log's flush. The bundled engine's quit waits (S110: a 1.5 s API drain and a 3 s graceful stop) went with `policy::engine` on 2026-10-20.
