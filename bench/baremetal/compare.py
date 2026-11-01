#!/usr/bin/env python3
"""Sandstorm against other open-source sandboxes, on one machine with KVM.

    sudo python3 compare.py --runtimes process,runc,sandstorm-fc-warm --apps web,app64 --out out.jsonl

Every runtime runs the same two programs from bench/fleet/apps: `web`, a
Python HTTP server, and `app64`, one holding a 64 MiB numpy heap that every
request reads. For each runtime and program:

    start   one instance at a time, 20 of them after one to warm up: from the
            call that starts it to its first 200, polled every 5 ms
    memory  100 kept running, each answering before the next starts; then
            30 s to settle, MemAvailable against before, and 20 of them
            called at random to check they all still answer

`memory` is per instance on top of whatever the runtime holds resident with
nothing running (its daemons), which is not counted: MemAvailable is read with the runtime already up. 100
rather than until the machine is full: Docker's bridge holds 1023 ports, and
a 31 GiB machine would take thousands of the lighter ones.

The runtimes from bench/fleet/baselines.py (process, bwrap, runc, runsc,
podman) run the program from the host's /usr; Kata does too, as a Docker
runtime. Sandstorm runs it the way a template does, and microsandbox from an
image of the same rootfs (see setup.sh).
"""

import argparse
import json
import os
import random
import ssl
import statistics
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "fleet"))
import baselines as B  # noqa: E402

POLL = 0.005


def poll(url, headers, deadline):
    """Seconds until url answers 200, or None. Polled every 5 ms."""
    req = urllib.request.Request(url, headers=headers or {})
    t0 = time.perf_counter()
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(req, timeout=2) as r:
                r.read()
                if r.status == 200:
                    return time.perf_counter() - t0
        except Exception:
            time.sleep(POLL)
    return None


# --- runtimes ------------------------------------------------------------------
#
# start(app, i) -> (handle, url, headers); stop(handle); stop_all(). `idle` is
# brought up before and down after each runtime, so one's daemons are never
# counted against another.


class Baseline:
    """A runtime from baselines.py, which has no per-instance stop."""

    def __init__(self, cls, a):
        self.name, self.rt = cls.name, cls(a)

    def start(self, app, i):
        h, url = self.rt.start(app, i)
        return h, url, None

    def stop(self, h):
        pass

    def stop_all(self):
        self.rt.stop_all()


class Kata(B.Container):
    name, runtime = "kata", "io.containerd.kata.v2"


class Sandstorm:
    """A local master and node; templates registered by setup.sh."""

    def __init__(self, name, master, pki, warm):
        self.name, self.master, self.warm = name, master, warm
        self.token = (Path(pki) / "api-token").read_text().strip()
        self.ctx = ssl.create_default_context(cafile=str(Path(pki) / "ca.pem"))
        self.ctx.check_hostname = False
        self.ids = []

    def call(self, method, path, body=None):
        req = urllib.request.Request(
            self.master + path,
            data=json.dumps(body).encode() if body is not None else None,
            method=method,
            headers={"authorization": f"Bearer {self.token}", "content-type": "application/json"},
        )
        with urllib.request.urlopen(req, context=self.ctx, timeout=120) as r:
            raw = r.read()
            return json.loads(raw) if raw else None

    def start(self, app, i):
        sb = self.call("POST", "/v1/sandboxes", {"template": app + ("-warm" if self.warm else ""), "port": 8000})
        self.ids.append(sb["id"])
        return sb["id"], f"http://127.0.0.1:{sb['gateway_port']}/", {"x-sandstorm-sandbox-token": sb["token"]}

    def stop(self, h):
        self.call("DELETE", f"/v1/sandboxes/{h}")
        self.ids.remove(h)

    def stop_all(self):
        for h in list(self.ids):
            self.stop(h)


class Microsandbox:
    """`msb run -d` from images setup.sh loaded, the port forwarded to the host.
    (`msb create` boots the VM without running the image's command.)"""

    name = "microsandbox"

    def __init__(self, a):
        self.names = []

    def start(self, app, i):
        name, port = f"cmp-{app}-{i}-{random.randrange(1 << 30)}", 31000 + i
        subprocess.run(["msb", "run", "-d", "-q", "-n", name, "-m", "512", "-p", f"{port}:8000", f"sst-{app}:latest"],
                       check=True, capture_output=True)
        self.names.append(name)
        return name, f"http://127.0.0.1:{port}/", None

    def stop(self, h):
        subprocess.run(["msb", "rm", "-f", h], capture_output=True)
        self.names.remove(h)

    def stop_all(self):
        if self.names:
            subprocess.run(["msb", "rm", "-f"] + self.names, capture_output=True)
        self.names = []


class CubeSandbox:
    """CubeSandbox through its E2B-compatible API, reached through cube-proxy.

    Templates are made by setup-cube.sh from the same rootfs, as an image."""

    name = "cubesandbox"

    def __init__(self, a):
        self.templates = {"web": a.cube_web, "app64": a.cube_app64}
        self.ids = []

    def call(self, method, path, body=None):
        req = urllib.request.Request(
            "http://127.0.0.1:3000" + path,
            data=json.dumps(body).encode() if body is not None else None,
            method=method,
            headers={"X-API-Key": "e2b_000000", "Content-Type": "application/json"},
        )
        with urllib.request.urlopen(req, timeout=120) as r:
            raw = r.read()
            return json.loads(raw) if raw else None

    def start(self, app, i):
        sb = self.call("POST", "/sandboxes", {"templateID": self.templates[app], "timeout": 3600})
        self.ids.append(sb["sandboxID"])
        return sb["sandboxID"], "http://127.0.0.1/", {"Host": f"8000-{sb['sandboxID']}.cube.app"}

    def stop(self, h):
        try:
            self.call("DELETE", f"/sandboxes/{h}")
        except Exception:
            pass
        self.ids.remove(h)

    def stop_all(self):
        for h in list(self.ids):
            self.stop(h)


def make(name, a):
    if name in B.RUNTIMES:
        return Baseline(B.RUNTIMES[name], a)
    if name == "kata":
        return Baseline(Kata, a)
    if name == "microsandbox":
        return Microsandbox(a)
    if name == "cubesandbox":
        return CubeSandbox(a)
    if name.startswith("sandstorm-"):
        backend = "fc" if "-fc" in name else "gvisor"
        return Sandstorm(name, a.fc_master if backend == "fc" else a.gvisor_master,
                         a.fc_pki if backend == "fc" else a.gvisor_pki, name.endswith("-warm"))
    raise SystemExit(f"unknown runtime {name}")


# --- the measurements -------------------------------------------------------------


def stop_all(rt, out):
    """Clean up, without losing the measurement if cleanup fails: Kata left a
    container `docker rm` could not kill, its process a zombie."""
    try:
        rt.stop_all()
    except Exception as e:
        out.write(json.dumps({"runtime": rt.name, "kind": "cleanup_failed", "error": str(e)[-400:]}) + "\n")


def start_times(rt, app, n, out):
    took = []
    for i in range(n + 1):
        t0 = time.perf_counter()
        h, url, hdr = rt.start(app, i)
        s = poll(url, hdr, time.time() + 120)
        total = time.perf_counter() - t0 if s is not None else None
        rt.stop(h)
        if i:  # the first warms caches and templates up
            took.append(total)
            out.write(json.dumps({"runtime": rt.name, "app": app, "kind": "start", "n": i,
                                  "start_to_answer_s": total and round(total, 4)}) + "\n")
    stop_all(rt, out)
    ok = sorted(t * 1000 for t in took if t is not None)
    return {"p50_ms": round(statistics.median(ok), 1), "min_ms": round(ok[0], 1),
            "p90_ms": round(ok[int(0.9 * len(ok))], 1), "failed": len(took) - len(ok)} if ok else {"failed": len(took)}


def memory(rt, app, n, out):
    base = B.avail_mib()
    up = []
    failed = None
    for i in range(n):
        try:
            h, url, hdr = rt.start(app, 1000 + i)
        except Exception as e:  # a runtime that runs out of something says so here
            failed = str(e)[-400:]
            out.write(json.dumps({"runtime": rt.name, "app": app, "kind": "start_failed", "n": i, "error": failed}) + "\n")
            break
        if poll(url, hdr, time.time() + 120) is None:
            out.write(json.dumps({"runtime": rt.name, "app": app, "kind": "never_answered", "n": i}) + "\n")
        up.append((h, url, hdr))
        if B.avail_mib() < B.FLOOR_MIB:
            break
    time.sleep(30)
    after = B.avail_mib()
    sample = random.sample(up, min(20, len(up)))
    answered = sum(poll(u, hd, time.time() + 30) is not None for _, u, hd in sample)
    stop_all(rt, out)
    return {"kept": len(up), "per_instance_mib": round((base - after) / len(up), 1),
            "sampled": len(sample), "answered": answered, **({"stopped_by": failed} if failed else {})}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--runtimes", required=True)
    ap.add_argument("--apps", default="web,app64", dest="programs")
    ap.add_argument("--start-count", type=int, default=20)
    ap.add_argument("--keep", type=int, default=100)
    ap.add_argument("--skip", default="", help="start or memory")
    ap.add_argument("--app-dir", default="/root/rootfs/app")
    ap.add_argument("--gvisor-master", default="https://127.0.0.1:18080")
    ap.add_argument("--gvisor-pki", default="/var/lib/sst-gvisor/pki")
    ap.add_argument("--fc-master", default="https://127.0.0.1:18081")
    ap.add_argument("--fc-pki", default="/var/lib/sst-fc/pki")
    ap.add_argument("--cube-web", default="")
    ap.add_argument("--cube-app64", default="")
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    a.apps = a.app_dir  # the directory baselines.py's runtimes bind in
    out = open(a.out, "a", buffering=1)
    for name in a.runtimes.split(","):
        docker = name in ("runc", "runsc", "kata")
        if docker:
            B.docker(True)
        rt = make(name, a)
        for app in a.programs.split(","):
            row = {"runtime": name, "app": app, "kind": "summary", "avail_mib_before": B.avail_mib()}
            if "start" not in a.skip:
                row["start"] = start_times(rt, app, a.start_count, out)
            if "memory" not in a.skip:
                row["memory"] = memory(rt, app, a.keep, out)
            out.write(json.dumps(row) + "\n")
            print(json.dumps(row), flush=True)
        if docker:
            B.docker(False)


if __name__ == "__main__":
    main()
