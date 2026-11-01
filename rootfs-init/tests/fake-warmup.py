#!/usr/bin/env python3
"""Fake warmup process for testing sandstorm-agent's bridge logic on the host.

Speaks the line-JSON protocol of a real warm-up process, such as a
sandstorm-warmup.js, but doesn't actually launch a browser. Used to smoke-
test the bridge without needing a full Firecracker + rootfs round-trip.

Protocol:
    ready handshake: {"ready": true} on stdout, once
    request:         {"id": ..., "code": "<js-or-anything>"} on stdin
    reply:           {"id": ..., "result": "echoed: <code>"}

This file is NOT shipped in any production rootfs — it lives here so
the agent's bridge code path can be exercised on a plain Linux host.
"""

import json
import sys


def main() -> None:
    sys.stderr.write("fake-warmup: started\n")
    sys.stderr.flush()
    sys.stdout.write(json.dumps({"ready": True}) + "\n")
    sys.stdout.flush()
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            req = json.loads(line)
        except Exception as e:
            sys.stdout.write(json.dumps({"error": f"bad json: {e}"}) + "\n")
            sys.stdout.flush()
            continue
        sys.stdout.write(
            json.dumps({"id": req.get("id"), "result": f"echoed: {req.get('code')}"})
            + "\n"
        )
        sys.stdout.flush()


if __name__ == "__main__":
    main()
