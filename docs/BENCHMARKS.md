# Benchmarks and soak test

Two scripts keep an eye on the engine's own audio path over long runs: `Tests/bench.sh` records a baseline, and `Tests/soak.sh` plays for an hour and checks the result against that baseline. Both run unattended and make no sound.

## What they cover

The part of the engine that runs all the time while you listen:

- the delay ring buffer and the `OutputRenderer`s that play it on local outputs, each at its own delay with drift correction;
- the `FifoFeed` that streams PCM16 at 48 kHz into the named pipe the AirPlay engine reads.

Capture itself isn't in it (a process tap needs the recording permission and real audio), and neither is OwnTone or a real AirPlay speaker. A producer thread stands in for the tap: it writes 512-frame blocks paced by the system's uptime clock, into a ring buffer the same size as the app's. A paced reader on the pipe stands in for the engine.

## Staying silent

Every local output gets gain 0 before its first render callback can run, so the audio that reaches the device is all zeros. The harness picks virtual outputs first and then built-in ones, and never an AirPlay output, a Bluetooth device or an input device. You can name outputs with `--device`, but AirPlay is refused. Nothing changes system audio settings: no default device, sample rate or volume. `caffeinate -i` keeps the Mac from idle-sleeping while a script runs.

## How it measures

The signal is a frame counter, 14 bits in each channel and spaced so the Float to Int16 rounding can't change it. The pipe reader decodes it, so it knows exactly which frame arrived and when audio was dropped or repeated.

Every 10 seconds the harness writes one record to `windows.jsonl` and `windows.csv`:

- Lag error per output: how far the output trails the newest written frame, minus its target delay (2000 ms during the run). Sampled every 100 ms; the record holds the window's mean, min and max. A few ms negative is normal, since the lag is read after each render block.
- Dropouts per output: the device's processor-overload notifications (`kAudioDeviceProcessorOverload`), counted through a property listener. Each one means the HAL missed an IO cycle on that output, which you'd hear as a glitch.
- Starved renders: render callbacks that ran into the write head and filled with silence. At the 2000 ms run delay a healthy run never gets one, because a capture stall turns into owed silence (a drift correction) long before the reader reaches the write head; it only happens with a delay inside the dead band. It's kept as a guard, not as the dropout measure.
- Drift corrections: times the reader skipped forward or inserted silence because its lag left the dead band (35 ms, or the observed timing jitter if larger). Resyncs (applied delay changes) are counted separately.
- Inter-output drift: lag of output 0 minus lag of output 1, the window mean, in ms.
- FIFO path: reader underruns (the reader keeps 200 ms read-ahead and plays at 48 kHz; an underrun is that buffer running dry, after which it rebuffers), gaps in the decoded counter, latency (newest frame produced minus newest frame received), and the writer's pipe-full waits, short writes, backlog skips and write errors.
- CPU % of the harness (from `getrusage`), its memory footprint, and how late the producer ran.

The counters come from read-only atomics in `DelayReader` and `FifoFeed`. They're relaxed atomic adds, so the render callback and the FIFO writer don't allocate, lock or log because of them, and they don't change what gets played.

`bench.sh` runs two configurations, `--minutes` each (60 by default, so a plain run takes two hours and the drift between two outputs is measured over a full hour):

- `two_local`: two local outputs. Before the run it sets each output to 150, 500, 2000 and 3000 ms and records the lag error at each (the same delays the hardware test checks).
- `local_plus_fifo`: one local output plus the FIFO path.

Real AirPlay is recorded as not available, since it needs a speaker on the network and makes sound. While each configuration runs, `ps` samples the harness's CPU % and RSS every second. `ps` averages CPU over time on macOS, so its mean comes out lower than the harness's own figure.

At the end of a run the harness checks that each output rendered at least 95% of the expected frames, that the pipe reader got at least 95%, that the producer never ran 200 ms late and that the run lasted its full time. A run that fails one of those fails, baseline or not.

## Running it

```bash
Tests/bench.sh     # 2 × 60 min: writes Tests/baseline/swift.json and the Swift results below
Tests/soak.sh      # 60 min, judged against that baseline
```

Both take `--minutes N` and `--out DIR` (by default the logs go to a new folder under `$TMPDIR`). For `bench.sh`, N is per configuration, at least 1. `soak.sh` also takes `--baseline FILE` and `--device NAME` (AirPlay, aggregate and multi-output devices are refused). `bench.sh` takes `--baseline-out FILE` and `--no-doc`; without `--no-doc` it rewrites the Swift results between the marker comments below, so the table always matches the baseline it came from. It needs `python3`, which comes with the Command Line Tools.

`summary.txt` in the output folder has the verdict. `soak.sh` exits 0 on PASS, 1 on FAIL, 2 when it can't start (the harness doesn't compile, fewer than two usable outputs, or a baseline that lacks a metric the run is judged on) and 3 when there's no baseline file. It warns, on stderr and in the summary, when the baseline came from another Mac model, another macOS version or other outputs. Ctrl-C stops the harness too.

`Tests/run-tests.sh` only compiles the harness; it never runs these.

## Thresholds

`soak.sh` plays through two local outputs and the FIFO path at once. It compares the local numbers with the baseline's `two_local` run and the FIFO numbers with `local_plus_fifo`:

- Event rates (local dropouts, local starved renders, local drift corrections, FIFO underruns, FIFO gaps): the allowed rate is max(2 × baseline, baseline + 5) per hour. The run passes when its count is at most that rate times the hours it ran, rounded up, so a short run still gets room for one event.
- Inter-output drift and local lag error (the worst 10-second mean, as an absolute value): at most the baseline plus 35 ms. That's one drift-correction dead band: each output is held within it of its target, so going past baseline plus 35 ms means correction isn't keeping up.
- FIFO latency: the maximum at most the baseline plus 50 ms, and the change from the first to the last window at most the baseline's change plus 20 ms. Latency that keeps growing is the leak the writer's 250 ms backlog cap exists to stop.
- Memory growth (first to last window) and mean CPU: at most max(2 × baseline, baseline + 5), in MB and percentage points.

## Results

The release baseline is a full `Tests/bench.sh` run (60 minutes per configuration). It hasn't been recorded yet; until it is, the Swift numbers below come from a short smoke run that only shows the scripts work, and `soak.sh` without a baseline exits 3.

The Rust column is reserved for the Rust core comparison; it stays empty until that engine exists.

<!-- bench:swift:begin -->
Run: 1 min per configuration, 2026-10-17, on d95ffb3.

Machine: MacBook Air (Mac16,12), Apple M4, 16 GB, macOS 26.6.2 (25G83). Outputs: Microsoft Teams Audio, MacBook Air Speakers; local + FIFO used Microsoft Teams Audio.

This is a short smoke run, not the release baseline; record that with a plain `Tests/bench.sh`.

| Metric | Swift | Rust |
|---|---|---|
| Delay error at 150 / 500 / 2000 / 3000 ms (ms, per output) | (+0.0, -2.1) / (-2.1, -3.2) / (-11.7, -3.7) / (-1.1, -3.7) | |
| Two local: dropouts per hour (worst output) | 0 | |
| Two local: starved renders per hour (worst output) | 0 | |
| Two local: drift corrections per hour (worst output) | 0 | |
| Two local: worst lag error, 10 s mean (ms) | 4.312 | |
| Two local: inter-output drift, mean / worst (ms) | 2.686 / 3.177 | |
| Two local: inter-output drift change, first to last window (ms) | -0.454 | |
| Two local: CPU % mean (harness / ps) | 0.651 / 0.23 | |
| Two local: memory footprint max / RSS max (MB) | 40.469 / 52.4 | |
| Local + FIFO: FIFO underruns per hour | 0 | |
| Local + FIFO: FIFO gaps per hour | 0 | |
| Local + FIFO: FIFO latency mean / max (ms) | 10.484 / 21.333 | |
| Local + FIFO: pipe-full waits, short writes, backlog skips | 0, 0, 0 | |
| Local + FIFO: local dropouts per hour | 0 | |
| Local + FIFO: CPU % mean (harness / ps) | 1.166 / 0.62 | |
| Real AirPlay | not available | |
<!-- bench:swift:end -->
