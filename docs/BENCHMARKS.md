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
- Underruns: render callbacks that ran into the write head and had to fill with silence.
- Drift corrections: times the reader skipped forward or inserted silence because its lag left the dead band (35 ms, or the observed timing jitter if larger). Resyncs (applied delay changes) are counted separately.
- Inter-output drift: lag of output 0 minus lag of output 1, the window mean, in ms.
- FIFO path: reader underruns (the reader keeps 200 ms read-ahead and plays at 48 kHz; an underrun is that buffer running dry, after which it rebuffers), gaps in the decoded counter, latency (newest frame produced minus newest frame received), and the writer's pipe-full waits, short writes, backlog skips and write errors.
- CPU % of the harness (from `getrusage`), its memory footprint, and how late the producer ran.

The counters come from read-only atomics in `DelayReader` and `FifoFeed`. They're relaxed atomic adds, so the render callback and the FIFO writer don't allocate, lock or log because of them, and they don't change what gets played.

`bench.sh` runs two configurations, half the time each:

- `two_local`: two local outputs. Before the run it sets each output to 150, 500, 2000 and 3000 ms and records the lag error at each (the same delays the hardware test checks).
- `local_plus_fifo`: one local output plus the FIFO path.

Real AirPlay is recorded as not available, since it needs a speaker on the network and makes sound. While each configuration runs, `ps` samples the harness's CPU % and RSS every second. `ps` averages CPU over time on macOS, so its mean comes out lower than the harness's own figure.

At the end of a run the harness checks that each output rendered at least 95% of the expected frames, that the pipe reader got at least 95%, that the producer never ran 200 ms late and that the run lasted its full time. A run that fails one of those fails, baseline or not.

## Running it

```bash
Tests/bench.sh     # 60 min, writes Tests/baseline/swift.json
Tests/soak.sh      # 60 min, judged against that baseline
```

Both take `--minutes N` and `--out DIR` (by default the logs go to a new folder under `$TMPDIR`). `soak.sh` also takes `--baseline FILE` and `--device NAME`; `bench.sh` takes `--baseline-out FILE`. `summary.txt` in the output folder has the verdict. `soak.sh` exits 0 on PASS (or when there's no baseline yet, which it says), 1 on FAIL and 2 when it can't start, for example with fewer than two usable outputs.

`Tests/run-tests.sh` only compiles the harness; it never runs these.

## Thresholds

`soak.sh` plays through two local outputs and the FIFO path at once. It compares the local numbers with the baseline's `two_local` run and the FIFO numbers with `local_plus_fifo`:

- Event rates (local underruns, local drift corrections, FIFO underruns, FIFO gaps): the allowed rate is max(2 × baseline, baseline + 5) per hour. The run passes when its count is at most that rate times the hours it ran, rounded up, so a short run still gets room for one event.
- Inter-output drift and local lag error (the worst 10-second mean, as an absolute value): at most the baseline plus 35 ms. That's one drift-correction dead band: each output is held within it of its target, so going past baseline plus 35 ms means correction isn't keeping up.
- FIFO latency: the maximum at most the baseline plus 50 ms, and the change from the first to the last window at most the baseline's change plus 20 ms. Latency that keeps growing is the leak the writer's 250 ms backlog cap exists to stop.
- Memory growth (first to last window) and mean CPU: at most max(2 × baseline, baseline + 5), in MB and percentage points.

## Results

The baseline for releases is a 60-minute `bench.sh` run. It hasn't been recorded yet; until it is, `soak.sh` reports numbers and says there's no baseline.

Machine: MacBook Air (Mac16,12), Apple M4, 16 GB, macOS 26.6.2 (25G83). Commit: 3a5ac2f plus this change, uncommitted at the time.

The Swift column below is a 2-minute smoke run (1 minute per configuration). It only shows the scripts work; don't use these numbers as the baseline.

| Metric | Swift (smoke, 2 min) | Rust |
|---|---|---|
| Delay error at 150 ms (ms, per output) | -6.9, -8.5 | |
| Delay error at 500 ms | -6.4, -8.5 | |
| Delay error at 2000 ms | -5.9, -9.6 | |
| Delay error at 3000 ms | -4.8, -8.5 | |
| Two local: underruns per hour (worst output) | 0 | |
| Two local: drift corrections per hour (worst output) | 0 | |
| Two local: worst lag error, 10 s mean (ms) | 9.191 | |
| Two local: inter-output drift, mean / worst (ms) | 2.675 / 3.785 | |
| Two local: inter-output drift change over the run (ms) | 0.957 | |
| Two local: CPU % mean (harness / ps) | 0.697 / 0.32 | |
| Two local: memory footprint max / RSS max (MB) | 40.438 / 47.6 | |
| Local + FIFO: FIFO underruns per hour | 0 | |
| Local + FIFO: FIFO gaps per hour | 0 | |
| Local + FIFO: FIFO latency mean / max (ms) | 17.435 / 26.896 | |
| Local + FIFO: pipe-full waits, short writes, backlog skips | 0, 0, 0 | |
| Local + FIFO: local underruns per hour | 0 | |
| Local + FIFO: CPU % mean (harness / ps) | 1.163 / 0.67 | |
| Real AirPlay | not available | |

Delay errors list the virtual output first, then the built-in speakers.

The 60-minute Swift baseline: _not recorded yet (placeholder for the first full `Tests/bench.sh` run)._
