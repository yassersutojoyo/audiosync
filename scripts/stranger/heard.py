#!/usr/bin/env python3
"""Click recordings for the stranger workflow (.github/workflows/stranger.yml).

  heard.py clicks <out.wav> [--seconds S] [--rate R]
      A stereo 16-bit click train: 30 ms of 1 kHz, the gaps cycling
      3, 4 and 5 s, so every click is told apart from the next two and an
      extra copy at any delay breaks the pattern.
  heard.py record <out.wav> <out.stamps> [--rate R]
      Raw s16le stereo from stdin into a WAV, with one stamp line per chunk:
      the frames read so far and CLOCK_MONOTONIC at that moment.
  heard.py check --receiver <wav> <stamps> --sink <wav> <stamps> [--min-clicks N]
                 [--max-offset-ms MS] [--label TEXT]
      Each recording must hold the click train once: at least N clicks, with
      every gap on the 3, 4, 5 s cycle. Prints the receiver's offset after
      the sink (median over matched clicks); with --max-offset-ms, an offset
      further from 0 than MS, or none, fails. Exits 1 on a failed check.
  heard.py --self-test

Standard library only.
"""

import array
import math
import os
import signal
import statistics
import sys
import time
import wave

CYCLE = (3.0, 4.0, 5.0)
CLICK_S = 0.03
FIRST_S = 1.0
GAP_TOLERANCE_S = 0.03
QUIET_S = 0.5
FLOOR = 1000


def click_starts(seconds):
    starts, t, i = [], FIRST_S, 0
    while t + CLICK_S < seconds:
        starts.append(t)
        t += CYCLE[i % len(CYCLE)]
        i += 1
    return starts


def click_train(seconds, rate):
    samples = array.array("h", bytes(2 * int(seconds * rate)))
    length = int(CLICK_S * rate)
    ramp = max(1, length // 10)
    for start in click_starts(seconds):
        first = int(start * rate)
        for n in range(length):
            gain = min(1.0, n / ramp, (length - n) / ramp)
            samples[first + n] = int(16000 * gain * math.sin(2 * math.pi * 1000 * n / rate))
    return samples


def write_wav(path, mono, rate):
    stereo = array.array("h", bytes(4 * len(mono)))
    stereo[0::2] = mono
    stereo[1::2] = mono
    with wave.open(path, "wb") as out:
        out.setnchannels(2)
        out.setsampwidth(2)
        out.setframerate(rate)
        out.writeframes(stereo.tobytes())


def read_wav(path):
    with wave.open(path, "rb") as source:
        if source.getsampwidth() != 2:
            raise SystemExit(f"{path}: not 16-bit")
        channels, rate = source.getnchannels(), source.getframerate()
        data = array.array("h", source.readframes(source.getnframes()))
    if sys.byteorder == "big":
        data.byteswap()
    return data[0::channels], rate


def read_stamps(path):
    stamps = []
    with open(path, encoding="ascii") as lines:
        for line in lines:
            frames, ns = line.split()
            stamps.append((int(frames), int(ns) / 1e9))
    return stamps


def find_onsets(mono, rate):
    """Frames where a click starts: a loud sample after QUIET_S of quiet.
    None when the peak is under FLOOR."""
    peak = max((abs(s) for s in mono), default=0)
    if peak < FLOOR:
        return []
    threshold = max(FLOOR, peak * 0.3)
    quiet = int(QUIET_S * rate)
    found, last = [], None
    for index, sample in enumerate(mono):
        if sample >= threshold or -sample >= threshold:
            if last is None or index - last > quiet:
                found.append(index)
            last = index
    return found


def phase(gaps):
    """The cycle position of the first gap when every gap fits, else None."""
    for start in range(len(CYCLE)):
        if all(abs(gap - CYCLE[(start + i) % len(CYCLE)]) <= GAP_TOLERANCE_S for i, gap in enumerate(gaps)):
            return start
    return None


def heard_once(frames, rate, min_clicks):
    """(None, phase) when the clicks are the train once, else (why, None)."""
    if len(frames) < min_clicks:
        return f"{len(frames)} clicks, at least {min_clicks} expected", None
    gaps = [(b - a) / rate for a, b in zip(frames, frames[1:])]
    start = phase(gaps)
    if start is None:
        shown = ", ".join(f"{gap:.3f}" for gap in gaps)
        return f"gaps {shown} s are not the 3, 4, 5 s cycle: heard twice or a click lost", None
    return None, start


def frame_time(stamps, frame, rate):
    for end, at in stamps:
        if end > frame:
            return at - (end - frame) / rate
    end, at = stamps[-1]
    return at - (end - frame) / rate


def offset_ms(rx, sink):
    """Median of each receiver click's time after the same click on the sink.

    rx and sink are (times, phase) pairs; a click's identity is its cycle
    position, so a match lies within half a cycle either way."""
    period = sum(CYCLE)
    (rx_times, rx_start), (sink_times, sink_start) = rx, sink
    diffs = []
    for i, t in enumerate(rx_times):
        identity = (rx_start + i) % len(CYCLE)
        near = [t - s for j, s in enumerate(sink_times)
                if (sink_start + j) % len(CYCLE) == identity and abs(t - s) < period / 2]
        if near:
            diffs.append(min(near, key=abs))
    return statistics.median(diffs) * 1000 if diffs else None


def check(receiver, sink, min_clicks, max_offset_ms=None):
    """Lines to print and whether both recordings passed. Each recording is
    (mono samples, rate, stamps)."""
    lines, ok, timed = [], True, {}
    for name, (mono, rate, stamps) in (("receiver", receiver), ("sink", sink)):
        frames = find_onsets(mono, rate)
        why, start = heard_once(frames, rate, min_clicks)
        if why:
            ok = False
            lines.append(f"{name}: FAIL, {why}")
        else:
            lines.append(f"{name}: PASS, heard once ({len(frames)} clicks on the cycle)")
            timed[name] = ([frame_time(stamps, f, rate) for f in frames], start)
    if len(timed) == 2:
        offset = offset_ms(timed["receiver"], timed["sink"])
        lines.append("offset: no click matched" if offset is None
                     else f"offset: receiver {offset:.0f} ms after the sink (median)")
    else:
        offset = None
        lines.append("offset: not measured (a recording failed)")
    if max_offset_ms is not None:
        if offset is None or abs(offset) > max_offset_ms:
            ok = False
            lines.append(f"in sync: FAIL, the offset is not within {max_offset_ms:.0f} ms")
        else:
            lines.append(f"in sync: PASS, within {max_offset_ms:.0f} ms")
    return lines, ok


def record(wav_path, stamps_path, rate):
    stop = []
    signal.signal(signal.SIGTERM, lambda *_: stop.append(True))
    frames, pending = 0, b""
    with wave.open(wav_path, "wb") as out, open(stamps_path, "w", encoding="ascii") as stamps:
        out.setnchannels(2)
        out.setsampwidth(2)
        out.setframerate(rate)
        while not stop:
            try:
                chunk = os.read(0, 16384)
            except InterruptedError:
                continue
            if not chunk:
                break
            at = time.monotonic_ns()
            pending += chunk
            whole = len(pending) - len(pending) % 4
            if whole:
                out.writeframes(pending[:whole])
                frames += whole // 4
                pending = pending[whole:]
                stamps.write(f"{frames} {at}\n")
                stamps.flush()


def self_test():
    rate = 8000
    seconds = 40
    train = click_train(seconds, rate)
    stamps = [(n, n / rate) for n in range(rate // 10, len(train) + rate // 10, rate // 10)]

    def delayed(samples, by_s):
        shift = int(by_s * rate)
        return array.array("h", bytes(2 * shift)) + samples[: len(samples) - shift]

    def mixed(a, b):
        return array.array("h", (max(-32768, min(32767, x + y)) for x, y in zip(a, b)))

    late = delayed(train, 2.0)
    cases = (
        ("heard once", late, train, True),
        ("heard twice on the receiver", mixed(late, delayed(train, 3.3)), train, False),
        ("AudioSync's copy on the sink", late, mixed(train, late), False),
        ("silent receiver", array.array("h", bytes(len(train) * 2)), train, False),
    )
    failed = 0
    for name, rx, sink, expected in cases:
        lines, ok = check((rx, rate, stamps), (sink, rate, stamps), min_clicks=6)
        if ok != expected:
            failed += 1
            print(f"self-test {name}: expected {'PASS' if expected else 'FAIL'}, got {lines}")
        if name == "heard once" and not any(line.startswith("offset: receiver 2000 ms") for line in lines):
            failed += 1
            print(f"self-test {name}: offset wrong: {lines}")
    for limit, expected in ((30, False), (2030, True)):
        _, ok = check((late, rate, stamps), (train, rate, stamps), 6, max_offset_ms=limit)
        if ok != expected:
            failed += 1
            print(f"self-test offset limit {limit} ms: expected {'PASS' if expected else 'FAIL'}")
    early_lines, ok = check((train, rate, stamps), (delayed(train, 0.02), rate, stamps), 6, max_offset_ms=30)
    if not ok or not any(line.startswith("offset: receiver -20 ms") for line in early_lines):
        failed += 1
        print(f"self-test receiver 20 ms ahead of the sink: {early_lines}")
    print("heard.py self-test: " + (f"{failed} FAILED" if failed else f"{len(cases)} cases passed"))
    return 1 if failed else 0


def main(argv):
    if argv[:1] == ["--self-test"]:
        return self_test()
    if argv[:1] == ["clicks"] and len(argv) >= 2:
        seconds = float(argv[argv.index("--seconds") + 1]) if "--seconds" in argv else 45.0
        rate = int(argv[argv.index("--rate") + 1]) if "--rate" in argv else 48000
        write_wav(argv[1], click_train(seconds, rate), rate)
        print(f"{len(click_starts(seconds))} clicks in {seconds:.0f} s at {rate} Hz")
        return 0
    if argv[:1] == ["record"] and len(argv) >= 3:
        rate = int(argv[argv.index("--rate") + 1]) if "--rate" in argv else 48000
        record(argv[1], argv[2], rate)
        return 0
    if argv[:1] == ["check"] and "--receiver" in argv and "--sink" in argv:
        def recording(flag):
            at = argv.index(flag)
            mono, rate = read_wav(argv[at + 1])
            return mono, rate, read_stamps(argv[at + 2])
        min_clicks = int(argv[argv.index("--min-clicks") + 1]) if "--min-clicks" in argv else 6
        label = argv[argv.index("--label") + 1] if "--label" in argv else ""
        max_offset = float(argv[argv.index("--max-offset-ms") + 1]) if "--max-offset-ms" in argv else None
        lines, ok = check(recording("--receiver"), recording("--sink"), min_clicks, max_offset)
        for line in lines:
            print(f"{label}{': ' if label else ''}{line}")
        return 0 if ok else 1
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
