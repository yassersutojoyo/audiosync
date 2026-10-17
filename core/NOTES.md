# audiosync-core: design notes

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
