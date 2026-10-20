# Benchmarks and soak test

Two scripts keep an eye on the engine's own audio path over long runs: `Tests/bench.sh` records a baseline, and `Tests/soak.sh` plays for an hour and checks the result against that baseline. Both run unattended and make no sound.

## What they cover

The part of the engine that runs all the time while you listen:

- the delay ring buffer and the `OutputRenderer`s that play it on local outputs, each at its own delay with drift correction.

Capture itself isn't in it (a process tap needs the recording permission and real audio), and neither is the AirPlay sender or a real AirPlay speaker. A producer thread stands in for the tap: it writes 512-frame blocks paced by the system's uptime clock, into a ring buffer the same size as the app's. Until 2026-10-20 the harness also ran the core's PCM pipe feed into a paced local reader; that feed went with the bundled AirPlay engine it fed.

The ring, the outputs' renderers (delay readers, rate conversion, gain and fades) are the Rust core's (`core/`), made the way a session in the app makes them (see [DEVELOPMENT.md](DEVELOPMENT.md)). Until 2026-10-19 the scripts could also run the Swift engine the core replaced; its last baseline, from 2026-10-17, is in the git history of `Tests/baseline/swift.json`.

## Staying silent

Every local output gets gain 0 before its first render callback can run, so the audio that reaches the device is all zeros. The harness picks virtual outputs first and then built-in ones, and never an AirPlay output, a Bluetooth device or an input device. You can name outputs with `--device`, but AirPlay is refused. Nothing changes system audio settings: no default device, sample rate or volume. `caffeinate -i` keeps the Mac from idle-sleeping while a script runs.

## How it measures

The signal is a frame counter, 14 bits in each channel.

Every 10 seconds the harness writes one record to `windows.jsonl` and `windows.csv`:

- Lag error per output: how far the output trails the newest written frame, minus its target delay (2000 ms during the run). Sampled every 100 ms; the record holds the window's mean, min and max. A few ms negative is normal, since the lag is read after each render block.
- Dropouts per output: the device's processor-overload notifications (`kAudioDeviceProcessorOverload`), counted through a property listener. Each one means the HAL missed an IO cycle on that output, which you'd hear as a glitch.
- Starved renders: render callbacks that ran into the write head and filled with silence. At the 2000 ms run delay a healthy run never gets one, because a capture stall turns into owed silence (a drift correction) long before the reader reaches the write head; it only happens with a delay inside the dead band. It's kept as a guard, not as the dropout measure.
- Drift corrections: times the reader skipped forward or inserted silence because its lag left the dead band (35 ms, or the observed timing jitter if larger). Resyncs (applied delay changes) are counted separately.
- Inter-output drift: lag of output 0 minus lag of output 1, the window mean, in ms.
- CPU % of the harness (from `getrusage`), its memory footprint, and how late the producer ran.

The counters come from read-only atomics in the core's delay readers. They're relaxed atomic adds, so the render callback doesn't allocate, lock or log because of them, and they don't change what gets played. The lag is the newest frame written when it's sampled, minus the next frame the output will read.

`bench.sh` runs one configuration, `two_local`, for `--minutes` (60 by default, so the drift between two outputs is measured over a full hour): two local outputs, each first set to 150, 500, 2000 and 3000 ms to record the lag error at each (the same delays the hardware test checks). Until 2026-10-20 it also ran `local_plus_fifo`, one local output plus the pipe feed; the release baseline below no longer carries that run.

Real AirPlay is recorded as not available, since it needs a speaker on the network and makes sound. While the configuration runs, `ps` samples the harness's CPU % and RSS every second. `ps` averages CPU over time on macOS, so its mean comes out lower than the harness's own figure.

At the end of a run the harness checks that each output rendered at least 95% of the expected frames, that the producer never ran 200 ms late and that the run lasted its full time. A run that fails one of those fails, baseline or not.

## Running it

```bash
Tests/bench.sh   # 60 min: writes Tests/baseline/rust.json and the results below
Tests/soak.sh    # 60 min, judged against that baseline
```

Both take `--minutes N` and `--out DIR` (by default the logs go to a new folder under `$TMPDIR`), and both build the Rust core first, since the harness links it. N is at least 1. `soak.sh` also takes `--baseline FILE` (it warns when that baseline names another engine) and `--device NAME` (AirPlay, aggregate and multi-output devices are refused). `bench.sh` takes `--baseline-out FILE` and `--no-doc`; without `--no-doc` it rewrites the results between the marker comments below from the baseline, so the table always matches it. `--doc-only` just rewrites that table. It needs `python3`, which comes with the Command Line Tools.

`summary.txt` in the output folder has the verdict. `soak.sh` exits 0 on PASS, 1 on FAIL, 2 when it can't start (the harness doesn't compile, fewer than two usable outputs, or a baseline that lacks a metric the run is judged on) and 3 when there's no baseline file. It warns, on stderr and in the summary, when the baseline came from another Mac model, another macOS version or other outputs. Ctrl-C stops the harness too.

`Tests/run-tests.sh` only compiles the harness and checks its memory rule on made-up series (`Tests/soakrulestest.swift`); it never runs these.

## Thresholds

`soak.sh` plays through two local outputs and compares the numbers with the baseline's `two_local` run:

- Event rates (local dropouts, local starved renders, local drift corrections): the allowed rate is max(2 × baseline, baseline + 5) per hour. The run passes when its count is at most that rate times the hours it ran, rounded up, so a short run still gets room for one event.
- Inter-output drift and local lag error (the worst 10-second mean, as an absolute value): at most the baseline plus 35 ms. That's one drift-correction dead band: each output is held within it of its target, so going past baseline plus 35 ms means correction isn't keeping up.
- Memory growth and mean CPU: at most max(2 × baseline, baseline + 5), in MB and percentage points.
- Memory growth leaves out a warm-up. The first window is logged while the session's buffers are still getting their pages, so a healthy run climbs once in its first minute or two (about 30 MB in a 10-minute soak) and then stays flat. Growth is the last window's footprint minus the level the run settled at: the median of the last three windows of the first 2 minutes (the first half of a run shorter than 4 minutes). A climb that's over by then counts as nothing; a leak keeps climbing and still fails. It's the median rather than the warm-up's highest window, so one short spike in the warm-up can't raise the level and hide a later leak. `result.json` records the warm-up and the level it settled at as `footprint_warmup_s` and `footprint_mb_settled`.
- The release baseline below predates the warm-up: its memory growth (`footprint_mb_growth` in `Tests/baseline/rust.json`) still runs from the first window to the last. The next full `Tests/bench.sh` run on the Mac measures it again with the warm-up.

## Reading lag drift and drift corrections

In a 60-minute Mac run on 2026-10-20, the built-in speakers' lag error went from about -10 ms to -21 ms while the virtual output stayed put. That's a real clock difference: the speakers' clock runs a little fast against the host clock the producer follows. An output at the capture rate has no clock tracking, so its lag simply follows its clock until it leaves the 35 ms dead band. A clock 3 ppm fast loses about 10.8 ms an hour, which reproduces that run, and gets corrected first after about 2 hours (the run starts at about -11 ms), then about every 3 hours. The clock stamps that trim a converting output don't change this. They never run on an output at the capture rate, and they only run on a converting output whose delay sits near its lead, not at the 2000 ms bench delay.

The drift corrections a run counts come from producer stalls instead. When the producer stalls past the dead band and then catches up, every output steps back once (owed silence) during the stall and skips forward once when the late blocks land: two corrections each, the same on both outputs. A stall shorter than the dead band, or one the output's fade-out outlasts, costs nothing. To confirm, look up the window where `outN_corrections` went up in `windows.jsonl` or `windows.csv`: its `producer_late_ms` should show the stall.

`core/tests/drift_hour.rs` runs this hour fast-forwarded, with a clean run and with one stall, so both readings stay checked.

## Results

The release baseline is a full `Tests/bench.sh` run (60 minutes per configuration), recorded before the pipe feed was removed; only its `two_local` run is kept.

<!-- bench:results:begin -->
60 min per configuration, on main, 2026-10-19.

Machine: MacBook Air (Mac16,12), Apple M4, 16 GB, macOS 26.6.2 (25G83). Outputs: Microsoft Teams Audio, MacBook Air Speakers.

| Metric | Result |
|---|---|
| Delay error at 150 / 500 / 2000 / 3000 ms (ms, per output) | (-10.7, -4.3) / (-11.2, -4.3) / (-10.7, -4.8) / (+0.5, -1.6) |
| Two local: dropouts per hour (worst output) | 0 |
| Two local: starved renders per hour (worst output) | 0 |
| Two local: drift corrections per hour (worst output) | 6 |
| Two local: worst lag error, 10 s mean (ms) | 9.319 |
| Two local: inter-output drift, mean / worst (ms) | 6.605 / 9.432 |
| Two local: inter-output drift change, first to last window (ms) | 3.758 |
| Two local: CPU % mean (harness / ps) | 0.822 / 0.31 |
| Two local: memory footprint max / RSS max (MB) | 42.516 / 53.2 |
| Real AirPlay | not available |
<!-- bench:results:end -->
