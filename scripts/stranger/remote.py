#!/usr/bin/env python3
"""Switches an AirPlay speaker on through AudioSync's web remote, as a paired
phone would, for the stranger workflow.

  remote.py <debug log> <port> <name part> [--timeout S]

The app must run with AUDIOSYNC_DEBUG=1 and AUDIOSYNC_WEB_REMOTE=1, so its
debug log carries the pairing code. Pairs over loopback, picks the whole
system as the source, waits for a speaker whose name holds <name part>,
switches it on and waits until the state says it is on. Prints the speakers
it saw. Exits 1 when a step times out. Standard library only.
"""

import http.client
import json
import re
import sys
import time
import urllib.parse


class Remote:
    def __init__(self, port):
        self.port = port
        self.cookie = None

    def call(self, method, path, body=None):
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=10)
        headers = {}
        if self.cookie:
            headers["Cookie"] = self.cookie
        data = None
        if body is not None:
            data = json.dumps(body).encode()
            headers["Content-Type"] = "application/json"
            headers["Origin"] = f"http://127.0.0.1:{self.port}"
        connection.request(method, path, body=data, headers=headers)
        response = connection.getresponse()
        text = response.read().decode("utf-8", "replace")
        for header, value in response.getheaders():
            if header.lower() == "set-cookie":
                pair = value.split(";", 1)[0]
                if pair.split("=", 1)[1]:
                    self.cookie = pair
        connection.close()
        return response.status, text


def wait(what, timeout, step):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        found = step()
        if found:
            return found
        time.sleep(1)
    raise SystemExit(f"remote.py: timed out waiting for {what}")


def pairing_code(log_path):
    try:
        with open(log_path, encoding="utf-8", errors="replace") as log:
            codes = re.findall(r"remote: pairing code (\d+)", log.read())
    except FileNotFoundError:
        return None
    return codes[-1] if codes else None


def main(argv):
    if len(argv) < 3:
        print(__doc__, file=sys.stderr)
        return 2
    log_path, port, name_part = argv[0], int(argv[1]), argv[2]
    timeout = float(argv[argv.index("--timeout") + 1]) if "--timeout" in argv else 90.0
    remote = Remote(port)

    def paired():
        code = pairing_code(log_path)
        if not code:
            return None
        try:
            status, text = remote.call("POST", "/api/pair", {"code": code})
        except OSError:
            return None
        return status == 200 and text

    wait("the pairing code and a pairing", timeout, paired)
    print("remote: paired over loopback")

    def state():
        status, text = remote.call("GET", "/api/state")
        return json.loads(text) if status == 200 else None

    current = wait("the first state", timeout, state)
    system = next((s for s in current.get("sources", []) if s.get("kind") == "system"), None)
    if system and current.get("source_id") != system["id"]:
        status, text = remote.call("POST", "/api/source", {"id": system["id"]})
        print(f"remote: source {system['name']}: {status} {text}")

    def speaker():
        found = state()
        for item in (found or {}).get("speakers", []):
            if name_part in item.get("name", "") and item.get("available"):
                return item
        return None

    target = wait(f"a speaker named *{name_part}*", timeout, speaker)
    path = "/api/speakers/" + urllib.parse.quote(target["id"], safe="")
    status, text = remote.call("POST", path, {"on": True})
    print(f"remote: switch on {target['name']} ({target['kind']}): {status} {text}")

    def switched_on():
        found = state()
        return next((s for s in (found or {}).get("speakers", []) if s["id"] == target["id"] and s["on"]), None)

    wait(f"{target['name']} on", timeout, switched_on)
    for item in state().get("speakers", []):
        print(f"remote: speaker {item['name']!r} kind={item['kind']} on={item['on']} volume={item.get('volume')}")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
