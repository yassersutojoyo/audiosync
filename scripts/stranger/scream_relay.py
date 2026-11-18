"""Plays the Scream sound card's stream into a named output, for the Windows calibrate loop.

Scream sends what is played on it as UDP packets of a 5-byte format header
and 1152 bytes of PCM (its README, https://github.com/duncanthrax/scream).
The loop points it at 127.0.0.1 and this relay plays the PCM into VB-CABLE's
input, whose output is the microphone, so the Scream speaker is heard.

    python -I scripts/stranger/scream_relay.py <port> <output name part>
"""

import socket
import sys

import sounddevice


def rate_of(header):
    base = 44100 if header[0] & 0x80 else 48000
    return base * max(1, header[0] & 0x7F)


def main(argv):
    port, wanted = int(argv[0]), argv[1]
    wasapi = next(i for i, api in enumerate(sounddevice.query_hostapis()) if "WASAPI" in api["name"])
    device = next(
        i
        for i, d in enumerate(sounddevice.query_devices())
        if d["hostapi"] == wasapi and d["max_output_channels"] > 0 and wanted in d["name"]
    )
    receiver = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    receiver.bind(("127.0.0.1", port))
    packet = receiver.recv(2048)
    header = packet[:5]
    rate, bits, channels = rate_of(header), header[1], header[2]
    print(f"relay: {rate} Hz, {bits} bit, {channels} channels into {sounddevice.query_devices(device)['name']}", flush=True)
    if bits != 16:
        print(f"relay: {bits}-bit samples are not handled", flush=True)
        return 1
    stream = sounddevice.RawOutputStream(
        samplerate=rate,
        channels=channels,
        dtype="int16",
        device=device,
        latency="low",
        extra_settings=sounddevice.WasapiSettings(auto_convert=True),
    )
    stream.start()
    while True:
        if packet[:5] == header:
            stream.write(packet[5:])
        packet = receiver.recv(2048)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
