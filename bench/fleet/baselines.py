#!/usr/bin/env python3
"""The same apps without Sandstorm, on the same machine, for comparison.

    sudo python3 baselines.py --apps /tmp/apps --out out/baselines.jsonl
    sudo python3 baselines.py --runtimes podman --checkpoint --out out/criu.jsonl

Every runtime runs the app the way a Sandstorm template does: the host's
/usr, /lib, /lib64, /bin and /sbin read-only, /etc/alternatives for numpy's
BLAS, and the app from apps/. Where the runtime gives one, each instance has
its own network namespace and is reached at its own address, as a sandbox is.

For each runtime and app: start instances one at a time, each answering
before the next starts, until MemAvailable falls under 512 MiB -- the floor a
Sandstorm node holds back at -- and record how many fit, how long each took
from start to first answer, and how long one request takes with all of them
up. Nothing here tiers anything: this is what fits when everything is kept
in memory, which is the comparison that matters for a fleet that does not.

    process   the app as a plain process (no isolation)
    bwrap     bubblewrap: mount, pid, ipc and uts namespaces, host network
    runc      Docker, runc, a bridge network
    runsc     Docker with gVisor (runsc), the same isolation Sandstorm uses,
              without the tiering
    podman    Podman (crun), a bridge network

--checkpoint, on podman, checkpoints and restores app64 instances with CRIU:
the closest thing to Sandstorm's snapshot tier that ships with a standard
container runtime. Restore is timed to the first answer, as Sandstorm's is.

Output: one JSON line per instance started and one summary line per
runtime and app, the same shape workloads.py writes.
"""

import argparse
import json
import os
import shutil
import subprocess
import tempfile
import time
import urllib.request

FLOOR_MIB = 512
APPS = {"web": None, "app16": 16, "app64": 64, "app256": 256, "app512": 512, "app1024": 1024}
BINDS = ["/usr", "/lib", "/lib64", "/bin", "/sbin", "/etc/alternatives"]


def avail_mib():
    for line in open("/proc/meminfo"):
        if line.startswith("MemAvailable:"):
            return int(line.split()[1]) // 1024
    return 0


def sh(*cmd, check=True):
    return subprocess.run(cmd, capture_output=True, text=True, check=check).stdout.strip()


def answer(url, deadline):
    """Poll until the app answers 200. Returns seconds taken, or None."""
    t0 = time.perf_counter()
    while time.time() < deadline:
        try:
            with urllib.request.urlopen(url, timeout=2) as r:
                r.read()
                if r.status == 200:
                    return time.perf_counter() - t0
        except Exception:
            pass
        time.sleep(0.05)
    return None


def once(url):
    t0 = time.perf_counter()
    try:
        with urllib.request.urlopen(url, timeout=30) as r:
            r.read()
            return r.status, time.perf_counter() - t0
    except Exception:
        return 0, time.perf_counter() - t0


def app_args(app, port=None):
    """The command line inside the instance."""
    mib = APPS[app]
    if mib is None:
        return ["/usr/bin/python3", "/app/web.py"] + ([str(port)] if port else [])
    return ["/usr/bin/python3", "/app/srv.py", str(mib)] + ([str(port)] if port else [])


# --- runtimes ----------------------------------------------------------------
#
# Each has start(app, i) -> (handle, url) and stop_all(). Ports for the ones
# on the host network start at 30000.

class Process:
    name = "process"

    def __init__(self, a):
        self.apps, self.procs = a.apps, []

    def start(self, app, i):
        port = 30000 + i
        cmd = [c.replace("/app/", self.apps + "/") for c in app_args(app, port)]
        p = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.procs.append(p)
        return p, f"http://127.0.0.1:{port}/"

    def stop_all(self):
        for p in self.procs:
            p.kill()
        for p in self.procs:
            p.wait()
        self.procs = []


class Bwrap(Process):
    name = "bwrap"

    def start(self, app, i):
        port = 30000 + i
        cmd = ["bwrap"]
        for b in BINDS:
            if os.path.exists(b):
                cmd += ["--ro-bind", b, b]
        cmd += ["--ro-bind", self.apps, "/app", "--proc", "/proc", "--dev", "/dev", "--tmpfs", "/tmp",
                "--unshare-all", "--share-net", "--die-with-parent", "--"] + app_args(app, port)
        p = subprocess.Popen(cmd, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.procs.append(p)
        return p, f"http://127.0.0.1:{port}/"


class Container:
    """Docker or Podman, from an image that is only mount points."""

    cli, runtime, name = "docker", None, "runc"

    def __init__(self, a):
        self.apps, self.ids = a.apps, []
        self.image = "localhost/bench-base:latest"
        if self.cli == "docker":
            self.image = "bench-base:latest"
        root = tempfile.mkdtemp()
        for d in ["proc", "dev", "sys", "tmp", "etc/alternatives", "app", "usr", "lib", "lib64", "bin", "sbin", "root"]:
            os.makedirs(os.path.join(root, d), exist_ok=True)
        open(os.path.join(root, "etc/passwd"), "w").write("root:x:0:0:root:/root:/bin/sh\n")
        tar = subprocess.run(["tar", "-C", root, "-c", "."], capture_output=True, check=True).stdout
        subprocess.run([self.cli, "import", "-", self.image], input=tar, capture_output=True, check=True)
        shutil.rmtree(root)

    def run_args(self):
        args = [self.cli, "run", "-d", "--network", "bridge"]
        if self.runtime:
            args += ["--runtime", self.runtime]
        for b in BINDS:
            if os.path.exists(b):
                args += ["-v", f"{os.path.realpath(b)}:{b}:ro"]
        return args + ["-v", f"{self.apps}:/app:ro"]

    def start(self, app, i):
        cid = sh(*self.run_args(), self.image, *app_args(app))
        self.ids.append(cid)
        ip = ""
        for _ in range(100):
            ip = sh(self.cli, "inspect", "-f", "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}", cid)
            if ip:
                break
            time.sleep(0.05)
        return cid, f"http://{ip}:8000/"

    def stop_all(self):
        # `-t` is Podman's; Docker's rm refuses it and removes nothing,
        # which left a first round's containers holding the memory every
        # later measurement was taken against.
        if self.ids:
            rm = [self.cli, "rm", "-f"] + (["-t", "0"] if self.cli == "podman" else [])
            r = subprocess.run(rm + self.ids, capture_output=True, text=True)
            if r.returncode != 0:
                raise RuntimeError(f"{self.cli} rm failed: {r.stderr.strip()[-300:]}")
        self.ids = []


class Runc(Container):
    pass


# Docker's daemons stay resident and cost memory whether or not a container
# runs, so they are up only while a Docker runtime is measured -- counted
# against Docker, and not left behind to count against the next runtime.
DOCKER_UNITS = ["docker.socket", "docker.service", "containerd.service"]


def docker(up):
    subprocess.run(["systemctl", "start" if up else "stop"] + DOCKER_UNITS, capture_output=True)
    time.sleep(3)


class Runsc(Container):
    name, runtime = "runsc", "runsc"


class Podman(Container):
    cli, name = "podman", "podman"


RUNTIMES = {c.name: c for c in (Process, Bwrap, Runc, Runsc, Podman)}


# --- the measurements ---------------------------------------------------------

def fill(rt, app, out, cap):
    """Start instances until the machine is under the floor. Returns them."""
    started = []
    while len(started) < cap and avail_mib() >= FLOOR_MIB:
        i = len(started)
        t0 = time.perf_counter()
        try:
            handle, url = rt.start(app, i)
        except subprocess.CalledProcessError as e:
            out.write("start_failed", runtime=rt.name, app=app, n=i, error=(e.stderr or "")[-300:])
            break
        took = answer(url, time.time() + 120)
        if took is None:
            out.write("never_answered", runtime=rt.name, app=app, n=i)
            break
        out.write("started", runtime=rt.name, app=app, n=i, start_to_answer_s=round(time.perf_counter() - t0, 4),
                  avail_mib=avail_mib())
        started.append((handle, url))
    return started


# A measurement only counts against a machine that starts out clean. Every
# node in the 2026-10-28 runs had 1.3 GB or more available when idle.
CLEAN_MIB = 1100


def density(rt, app, out, cap):
    base = avail_mib()
    if base < CLEAN_MIB:
        out.write("not_clean", runtime=rt.name, app=app, avail_mib=base)
    started = fill(rt, app, out, cap)
    lat = sorted(once(u)[1] for _, u in started)
    out.write("density", runtime=rt.name, app=app, instances=len(started), avail_before_mib=base,
              avail_after_mib=avail_mib(),
              answer_p50_ms=round(lat[len(lat) // 2] * 1000, 2) if lat else None,
              answer_p99_ms=round(lat[int(0.99 * (len(lat) - 1))] * 1000, 2) if lat else None)
    rt.stop_all()
    time.sleep(10)


def checkpoint(rt, out, n):
    """CRIU checkpoint and restore of app64 under Podman."""
    started = []
    for i in range(n):
        cid, url = rt.start("app64", i)
        if answer(url, time.time() + 120) is None:
            out.write("never_answered", runtime=rt.name, app="app64", n=i)
            continue
        started.append(cid)
    for cid in started:
        before = avail_mib()
        t0 = time.perf_counter()
        r = subprocess.run(["podman", "container", "checkpoint", cid], capture_output=True, text=True)
        ck = time.perf_counter() - t0
        if r.returncode != 0:
            out.write("checkpoint_failed", id=cid[:12], error=r.stderr[-300:])
            continue
        freed = avail_mib() - before
        t0 = time.perf_counter()
        r = subprocess.run(["podman", "container", "restore", cid], capture_output=True, text=True)
        if r.returncode != 0:
            out.write("restore_failed", id=cid[:12], error=r.stderr[-300:])
            continue
        ip = sh("podman", "inspect", "-f", "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}", cid)
        took = answer(f"http://{ip}:8000/", time.time() + 120)
        out.write("criu", runtime=rt.name, app="app64", checkpoint_s=round(ck, 4), freed_mib=freed,
                  restore_to_answer_s=round(time.perf_counter() - t0, 4) if took is not None else None)
    rt.stop_all()


class Out:
    def __init__(self, path):
        self.f = open(path, "a", buffering=1)

    def write(self, kind, **row):
        self.f.write(json.dumps({"t": round(time.time(), 3), "kind": kind, **row}) + "\n")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--apps", default=os.path.join(os.path.dirname(os.path.abspath(__file__)), "apps"))
    ap.add_argument("--runtimes", default="process,bwrap,runc,runsc,podman")
    ap.add_argument("--only", default=",".join(APPS), help="apps to measure")
    ap.add_argument("--cap", type=int, default=600, help="stop at this many even if more would fit")
    ap.add_argument("--checkpoint", action="store_true", help="podman: CRIU checkpoint/restore instead")
    ap.add_argument("--count", type=int, default=10, help="--checkpoint: instances")
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    a.apps = os.path.abspath(a.apps)
    out = Out(a.out)
    out.write("start", cmd="baselines", args=vars(a), kernel=os.uname().release, avail_mib=avail_mib())
    for name in a.runtimes.split(","):
        docker(RUNTIMES[name].cli == "docker" if hasattr(RUNTIMES[name], "cli") else False)
        rt = RUNTIMES[name](a)
        if a.checkpoint:
            checkpoint(rt, out, a.count)
            continue
        for app in a.only.split(","):
            density(rt, app, out, a.cap)
    docker(False)
    out.write("end")


if __name__ == "__main__":
    main()
