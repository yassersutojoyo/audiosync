#!/usr/bin/env python3
"""Recording windows for .github/workflows/silent-output-windows.yml.

  scream_window.py record <port> <seconds> <prefix>
      What the Scream sound card plays, as its UDP stream to 127.0.0.1:<port>,
      for <seconds>: <prefix>.wav (16-bit stereo), <prefix>.stamps in
      heard.py's format, and <prefix>.window, the window's start and end on
      the monotonic clock heard.py record stamps with.
  scream_window.py cut <wav> <stamps> <window> <prefix>
      The chunks of a heard.py recording stamped inside <window>, as
      <prefix>.wav and <prefix>.stamps.
  scream_window.py quiet <wav> <dbfs>
      Exits 1 unless every sample is below <dbfs> (an empty file passes:
      Scream sends nothing while nothing plays).

Standard library only.
"""

import array
import math
import socket
import sys
import time
import wave

HEADER = 5


def to_s16_stereo(packet):
    """Scream's header names the rate, the sample width and the channels."""
    rate_byte, bits, channels = packet[0], packet[1], packet[2]
    rate = (44100 if rate_byte & 0x80 else 48000) * max(1, rate_byte & 0x7F)
    width = bits // 8
    body = packet[HEADER:]
    frame = width * channels
    out = bytearray()
    for at in range(0, len(body) - frame + 1, frame):
        for channel in range(2):
            pick = min(channel, channels - 1)
            sample = body[at + pick * width:at + (pick + 1) * width]
            out += sample[-2:]
    return rate, bytes(out)


def record(port, seconds, prefix):
    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    sock.bind(("127.0.0.1", port))
    sock.settimeout(0.2)
    start = time.monotonic_ns()
    end = start + int(seconds * 1e9)
    rate, frames, chunks, stamps = 48000, 0, [], []
    while time.monotonic_ns() < end:
        try:
            packet = sock.recv(65536)
        except socket.timeout:
            continue
        if len(packet) <= HEADER:
            continue
        rate, pcm = to_s16_stereo(packet)
        chunks.append(pcm)
        frames += len(pcm) // 4
        stamps.append(f"{frames} {time.monotonic_ns()}\n")
    with wave.open(f"{prefix}.wav", "wb") as out:
        out.setnchannels(2)
        out.setsampwidth(2)
        out.setframerate(rate)
        out.writeframes(b"".join(chunks))
    with open(f"{prefix}.stamps", "w", encoding="ascii") as out:
        out.writelines(stamps)
    with open(f"{prefix}.window", "w", encoding="ascii") as out:
        out.write(f"{start} {end}\n")
    print(f"{prefix}: {frames} frames at {rate} Hz from Scream in {seconds:.0f} s")


def cut(wav_path, stamps_path, window_path, prefix):
    with open(window_path, encoding="ascii") as source:
        start, end = (int(part) for part in source.read().split())
    with wave.open(wav_path, "rb") as source:
        rate = source.getframerate()
        data = source.readframes(source.getnframes())
    kept, frames, stamps, previous = [], 0, [], 0
    with open(stamps_path, encoding="ascii") as lines:
        for line in lines:
            upto, at = (int(part) for part in line.split())
            if start <= at <= end:
                kept.append(data[previous * 4:upto * 4])
                frames += upto - previous
                stamps.append(f"{frames} {at}\n")
            previous = upto
    with wave.open(f"{prefix}.wav", "wb") as out:
        out.setnchannels(2)
        out.setsampwidth(2)
        out.setframerate(rate)
        out.writeframes(b"".join(kept))
    with open(f"{prefix}.stamps", "w", encoding="ascii") as out:
        out.writelines(stamps)
    print(f"{prefix}: {frames} frames inside the window")


def quiet(wav_path, dbfs):
    with wave.open(wav_path, "rb") as source:
        samples = array.array("h", source.readframes(source.getnframes()))
    if sys.byteorder == "big":
        samples.byteswap()
    peak = max((abs(s) for s in samples), default=0)
    level = 20 * math.log10(peak / 32768) if peak else float("-inf")
    ok = level < dbfs
    print(f"{wav_path}: peak {level:.1f} dBFS over {len(samples) // 2} frames, "
          f"{'PASS' if ok else 'FAIL'} against {dbfs:.0f} dBFS")
    return 0 if ok else 1


def main(argv):
    if argv[:1] == ["record"] and len(argv) == 4:
        record(int(argv[1]), float(argv[2]), argv[3])
        return 0
    if argv[:1] == ["cut"] and len(argv) == 5:
        cut(*argv[1:])
        return 0
    if argv[:1] == ["quiet"] and len(argv) == 3:
        return quiet(argv[1], float(argv[2]))
    print(__doc__, file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
