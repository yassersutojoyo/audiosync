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
