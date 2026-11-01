#!/usr/bin/env python3
"""microsandbox's fork, measured as fork.py measures Sandstorm's.

    sudo python3 fork_msb.py --out fork.jsonl

`msb fork` copies a running sandbox; there is no separate snapshot step. A
child gets no port forward (`msb fork` takes none and `msb modify` cannot add
one), so it cannot be called from the host the way fork.py calls Sandstorm's
children through their gateways. The time here is therefore until `msb fork`
returns with the children running -- more lenient than fork.py, which waits
for each child's first answer. Each child is then checked from inside, with
`msb exec`, outside the timing.
"""

import argparse
import json
import statistics
import subprocess
import time
import urllib.request

SRC = "fk-src"


def msb(*args, check=True):
    return subprocess.run(["msb", *args], capture_output=True, text=True, check=check)


def answers(name):
    r = msb("exec", name, "--", "python3", "-c",
            "import urllib.request; print(urllib.request.urlopen('http://127.0.0.1:8000/', timeout=10).read().decode())",
            check=False)
    return r.returncode == 0 and r.stdout.strip() != ""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    msb("rm", "-f", SRC, check=False)
    msb("run", "-d", "-q", "-n", SRC, "-m", "512", "-p", "32600:8000", "sst-app64:latest")
    end = time.time() + 60
    while time.time() < end:
        try:
            urllib.request.urlopen("http://127.0.0.1:32600/", timeout=1).read()
            break
        except Exception:
            time.sleep(0.05)

    one, checked = [], 0
    for i in range(11):
        t = time.time()
        msb("fork", SRC, "-n", f"fk1-{i}")
        one.append((time.time() - t) * 1000)
        checked += answers(f"fk1-{i}")
        msb("rm", "-f", f"fk1-{i}", check=False)
        time.sleep(0.5)

    batches, failed = {}, 0
    for n in (10, 20):
        walls = []
        for r in range(3):
            names = [f"fk{n}-{r}-{i}" for i in range(n)]
            t = time.time()
            msb("fork", SRC, "--names", *names)
            walls.append((time.time() - t) * 1000)
            failed += sum(not answers(x) for x in names[:3])
            msb("rm", "-f", *names, check=False)
            time.sleep(3)
        batches[n] = walls
    msb("rm", "-f", SRC, check=False)

    row = {"runtime": "microsandbox", "kind": "fork", "template": "app64", "measured_to": "msb fork returns",
           "fork1_ms": round(statistics.median(one[1:]), 1), "fork1_children_answering": f"{checked}/11",
           **{f"fork{n}_wall_ms": round(statistics.median(w), 1) for n, w in batches.items()},
           **{f"fork{n}_rounds_ms": [round(x) for x in w] for n, w in batches.items()},
           "batch_children_checked_failed": failed}
    open(a.out, "a").write(json.dumps(row) + "\n")
    print(json.dumps(row), flush=True)


if __name__ == "__main__":
    main()
