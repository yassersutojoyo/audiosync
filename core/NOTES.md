# audiosync-core implementation notes

Readings chosen where `docs/ENGINE_SPEC.md` leaves room. Each one satisfies every Observed clause.

## Ring (`ring.rs`)

- **S38, S40**: a write is stored in chunks of at most M frames, and W is published (release store, then release fence) after each chunk. The writer therefore never stores outside the slots of `[W - C, W - C + M)`, whatever the call size. A write larger than C stores every frame, and the earlier ones are overwritten, so only the last C remain. This costs O(frames) and keeps the post-copy check sound.
- **S35**: `Ring::writer()` hands out the single writer token. A second one is refused until the first is dropped.
- **S39**: `reset()` only sets W to 0. The old samples stay in memory but can no longer be read.

## Reader (`reader.rs`)

Implements S43, S44 and S52-S54 as corrected in the 2026-10-18 spec revision (no `seenW`; a reader more than T ahead steps back even while W stands still), plus the new S158 and S159. The `docs/ENGINE_SPEC.md` in this commit predates that revision.

- **S41, S43, S133**: there are two entry points. `read(dst, N, D, T)` is S41's interface. `read_target(dst, N)` takes the resync flag with an acquire exchange before it loads D and T, so a delay change is never applied with a stale target.
- **S42**: N is clamped to `dst.len() / 2`. De is also clamped to at least 0, so an absurd N larger than `C - M` cannot make it negative.
- **S44, S55**: the overrun test `P < oldest` uses the unclamped `W - C + M`, so only frames that were really overwritten count. A negative P early in a session reads silence (step 3) and follows the dead-band rules instead of re-anchoring on every read, which keeps a steady fresh session at one prime (S158).
- **S44**: when several step-2 conditions hold, they are classified in this order: prime, resync, overrun, skip forward, step back. Every case sets `P = W - De`, so the order only affects the counters.
- **S40, S55**: when the post-copy check fails, the copied range is replaced by silence and the next read re-anchors at `W - De` as an overrun. F still moves past the rejected range, because those frames are gone.
- **S158**: an overrun counts as a drift skip of e frames (e is always positive there). One extra counter, `rejected_copies`, counts post-copy rejections.
- **S159**: `reset_prime` also clears a pending re-anchor. A read that takes both a prime reset and a resync counts only the prime.
- **S50**: T is computed in integers as `(35 × Rc + 500) / 1000`, which rounds half up (44.1 kHz gives 1544).
- **S3**: computed in f64 with `floor`. A non-finite delay returns `None` so the caller can ignore it; a negative delay counts as 0.

## Resampler (`resample.rs`)

- **S85**: a band-limited Kaiser-windowed sinc (β = 9, 20 zero crossings, cutoff at 0.97 of the lower Nyquist) with weights normalized to unit DC gain. Per-phase weight banks are used when they fit in 32 Ki weights, otherwise weights come from a table interpolated per frame. Both paths give identical output.
- **S85**: the output count is exactly `ceil(fed × out / in)`, which is never more than one frame above the exact ratio (the spec allows ±2). The content is delayed by H input frames (about 0.45 ms at 44.1 kHz) so the count keeps pace with the input.
- **S94**: `set_input_rate` and `reset` never allocate. Buffers are sized when the resampler is created, for every input rate from 8 to 384 kHz.
- **S125**: a frame with a non-finite sample is stored as silence and zeroes all earlier history. A sum that overflows to a non-finite value on huge finite input is written as 0. When the rates are equal, samples pass through bit-exact (S85) and S86 maps them.

## Conversion (`convert.rs`)

- **S86**: computed in f64 so the product is exact and ties are decided on it.
- **S63-S65**: `map_to_device` is offered for the renderer's output stage. It handles channel mapping, gain, and replaces anything non-finite with 0 after the gain.

## FIFO feed (`fifo.rs`)

- **S84**: the pipe path is the host's: its `PipeSink` opens it. `samplesQueued` is monotonic over the feed's life. Neither `start` nor clearing the queue resets it.
- **S87**: the queue holds packed 32-bit frames (L in the low half, R in the high half), so a frame can never be split.
- **S88, S129**: one consumer token per queue makes a second writer impossible, and `stop()` joins the thread. Writer buffers are allocated in `start()` on the caller's thread; the writer thread allocates nothing after that.
- **S89, S93**: the open is retried every 50 ms, a full pipe every 2 ms, and an empty queue is polled every 4 ms. Other write errors are handled like `EPIPE`, after one open interval.
- **S90**: unwritten audio means queued frames plus frames taken but not yet written. A partly written frame counts as one and is never cut. Discards remove the not-yet-started pending frames first (they are the oldest), then the queue head. The cap is enforced before every open attempt and every write.
- **S92**: after `EPIPE`, whole frames still pending go to the next connection. Only the rest of the frame being written is abandoned (S93).
- **S160** (2026-10-18 revision): `EINTR` counts as a write error, being "an error other than EAGAIN". A write that takes zero bytes counts as a short write and then waits like a full pipe. Two extra counters, `open_failures` and `bytes_abandoned`, follow the same rules. The producer-side `samplesQueued` (S84) and `frames_dropped` (S87) are separate accessors.

## Build

- `[profile.test] opt-level = 1`: the two-hour drift simulations then run in about a second. Debug assertions and overflow checks stay on.

Everything in this crate is written from `docs/ENGINE_SPEC.md` (clause IDs S1 to S156) and public documentation (Rust and Swift docs, the RBJ Audio EQ Cookbook, the cargo-deny docs). No other source in this repository was used.

The crate was built in two halves. This file covers the equalizer, volume and gain, input channel mapping, the host policies, the C interface, CI, the license gate and the spec coverage checker. The ring buffer, delay reader, resampler, PCM16 conversion and FIFO feed are the other half (`ring.rs`, `reader.rs`, `resample.rs`, `convert.rs`, `fifo.rs`).

## Modules

- `eq`: the ten-band equalizer and preamp (S22 to S34), split into `EqControl` (main thread) and `EqProcessor` (capture thread) that share only atomics.
- `volume`: the cubic volume curve, host-side volume rules, coalesced volume writes and the renderer's gain stage (S64, S65, S70 to S76).
- `channels`: an input device's channels to stereo (S14).
- `policy`: pure host decisions the Windows app can reuse: delays and the AirPlay sync rule, capture source and microphone permission, the capture check, sessions and output sync, the engine lifecycle and pipe playback, the speaker list, reachability and reconcile, pairing, speaker keys, groups, device lists, layout and notices.
- `ffi`: the C interface.

## C interface

Decision: a plain C ABI (`extern "C"` with `#[unsafe(no_mangle)]`) declared by the hand-written header `include/audiosync_core.h`. No UniFFI and no cbindgen.

Why:

- Real time. Audio crosses the boundary a whole buffer at a time, as a pointer plus a frame count, and is processed in place: no copy and no allocation. UniFFI passes buffers through its own serialized `RustBuffer`, which allocates and copies on every call, so it can't sit on a Core Audio callback.
- No runtime or build dependency. UniFFI needs its crate, generated bindings and a build step; cbindgen needs a tool in CI. The surface is 16 functions.
- Swift imports a C header directly through a module map, and a Windows host can use the same header or link the `rlib`.
- The header can't drift silently: a unit test in `src/ffi.rs` parses the header and the Rust definitions, maps the types, and fails when any prototype, `ASC_ABI_VERSION`, `ASC_EQ_BANDS` or `ASC_EQ_PRESETS` differs. CI runs it with the other tests, so "the header is current" is checked without a generator.

Contract (also in the header): `asc_eq_process` runs on one capture thread at a time; the other `asc_eq_*` calls on a handle run on one control thread at a time; the two touch different fields of the handle. Null pointers make a call do nothing. All `unsafe` code of the crate is in `src/ffi.rs`, every block with a SAFETY comment (`clippy::undocumented_unsafe_blocks` is on there); the other modules `#![forbid(unsafe_code)]`.

`ffi/smoke.sh` builds the static library and a small Swift program (`ffi/swift-smoke/`, module map plus `main.swift`) that calls the version, the volume curve, the presets and one equalizer pass through the header. CI runs it on macOS. It touches no audio device.

## Real-time checks

- A counting global allocator in the test module of `src/ffi.rs` (there so that every `unsafe` line stays in that file) proves that the equalizer (including picking up new settings while the control thread keeps publishing, and the non-finite reset path), `equalize_and_deliver`, the C process call, `GainStage`, `apply_gain` and `input_to_stereo` neither allocate nor free.
- The equalizer shares two coefficient sets of `AtomicU64` values plus SeqCst `active` and `reading` indices; there is no lock anywhere. Building fails on a target without native 64-bit atomics (S128).
- A unit test counts biquad steps: exactly 20 per frame (S127).
- `benches/eq.rs` (criterion): about 7.2 µs per 512-frame stereo block with all ten bands active on an Apple silicon Mac, under 0.1 % of the block's 10.7 ms at 48 kHz.

## Dependencies and licenses

- Runtime dependencies: none.
- Dev-dependencies: `criterion` 0.7 (benchmark) and `proptest` 1.11 (property tests), the same versions the other half uses. Both are MIT OR Apache-2.0.
- `deny.toml` allows only MIT, Apache-2.0, BSD-2-Clause, BSD-3-Clause, Zlib, ISC and Unicode-3.0 (Unicode-DFS-2016 isn't needed). `include-dev` and `include-build` hold test, benchmark and build tools to the same list; without them cargo-deny checks only the crate itself. CI installs cargo-deny 0.20.2, the version checked locally.

## Spec readings

Where the spec left room, these readings satisfy every Observed clause:

1. S106 access kinds: a PIN flag (`needs_auth_key`) needs a code; so does a password on an AirPlay 2 speaker. The code is asked as a password when the speaker has one. An AirPlay 1 speaker with a password and no PIN flag is a legacy password speaker. This satisfies both "availability: password asks for a code" and "availability: AirPlay 1 password unsupported".
2. S98 Retry clears the start history and returns to `stopped`; the host's next start then goes to `starting` ("retry from gaveUp → starting"). A start while `failed` or `gaveUp` is refused until Retry. The sixth start within 60 s is refused at start time and moves to `gaveUp`. Exits while stopped, restarting, failed or given up are ignored.
3. S102 follows the corrected rule (2026-10-18): the engine also "came back" when an answer comes from a different engine run than the previous answer, even with no failed poll in between and even while a switch-on is in flight. `EngineLifecycle::run` counts runs; each poll carries the run it was sent to. The first answer ever counts as came back, since the engine was unreachable before it.
4. S103 settle window: measured on when the "off" report was requested: before the success, or less than 3 s after it.
5. S106 to S108 clicks: the user's click is kept until a code answers it, or a cancel, turn-off or automatic turn-on forgets the speaker. Asking doesn't use it up, so a speaker that vanishes before its prompt is answered gets its prompt again within the 45 s window; once a code was entered, a later refusal gets a notice.
6. S23: a non-finite band gain or preamp counts as 0 dB; any finite value is applied as given.
7. S76, S124: a non-finite volume input is ignored, like a non-finite delay; `gain_for_position` treats NaN as 0 (silence).
8. S64: `GainStage` ramps linearly across the first callback after a gain change (the spec allows a ramp of one callback); its first callback plays at the target. Gain 0 gives positive zeros; negative or non-finite gains count as 0.
9. S26: a block holds `len / 2` frames; an odd trailing sample is left untouched.
10. S31: if a flip races the capture thread's copy, the processor keeps the previous coefficients for one more block. It never waits and never reads a torn set.
11. S152: names compare ignoring case (Unicode lowercase) with ASCII digit runs compared by value. This is a deterministic stand-in for the host's localized comparison.
12. S150: the default group name's text ("Group n") comes from the host as a formatter, so it stays localized.
13. S100: an empty computer name counts as unknown and hides nothing.

## What stays in the host

Some clauses are covered here only in their pure part:

- S100: decoding the engine's JSON (invalid JSON, a wrong shape or a top-level array give no speakers) stays in the host; `policy::reconcile::speakers` applies the rules to decoded records.
- S99: the HTTP calls; `policy::engine` decides play, stop and when to ask again.
- S149: the JSON round trip of a group is the host's persistence format.
- S152 and S153: SF Symbol icons and colors, and opening System Settings.
- S7 and S34: `EqProcessor::equalize_and_deliver` fixes the order; the host wires the ring and the FIFO feed to it.

Clauses only the host can implement are listed with a reason in `tests/spec_exempt.txt` (21 entries: Core Audio, the OwnTone process and its HTTP API, macOS privacy and appearance settings, shell tooling).

## Overlap with the other half

- The renderer's output stage maps stereo to a device's channels with a fixed gain (`convert::map_to_device`, S63). `volume::GainStage` and `volume::SharedGain` add the one-callback ramp and the lock-free shared gain; a host can map with gain 1.0 and then run the gain stage, or use `map_to_device` alone.
- Milliseconds to frames (S3) is `reader::delay_ms_to_frames`; `policy::delay` works in milliseconds and leaves the conversion to the renderer.

## Spec coverage checker

`tests/spec_coverage.rs` reads `../docs/ENGINE_SPEC.md`, collects every clause ID and scans `src/` and `tests/` for lines of the exact form `// covers: S44, S49`. Three checks always run: every clause carries an Observed or Required tag and the IDs run from S1 without gaps (S1); every `// covers:` line is well formed and names existing clauses; the exempt list is well formed. The full check, every clause covered or exempt and no exempt clause covered after all, is ignored until both halves are merged (`cargo test --test spec_coverage -- --ignored`).
