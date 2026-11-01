#!/usr/bin/env python3
"""A pretend fleet for working on the console.

    console-sim.py http://127.0.0.1:8898 <api-token> [nodes] [seconds]

Needs a master started with --insecure-transport, which takes the node id
from the poll body. Each simulated node does what a real one does with the
master's commands -- creates, tier moves, destroys, and execs and branches,
which the console's snapshot builder uses -- and reports what it
holds, its free memory and pressure, the occasional sandbox that will not
start, and wakes from traffic at its gateway. One node drops out for a
minute now and then, so the console has something to notice.

Nothing here touches a real machine. Stdlib only.
"""
import json
import math
import random
import sys
import threading
import time
import urllib.error
import urllib.request

BASE, TOKEN = sys.argv[1].rstrip("/"), sys.argv[2]
NODES = int(sys.argv[3]) if len(sys.argv) > 3 else 6
SECS = int(sys.argv[4]) if len(sys.argv) > 4 else 6 * 3600
H = {"authorization": "Bearer " + TOKEN, "content-type": "application/json"}
START = time.time()


def call(method, path, body=None, timeout=10):
    req = urllib.request.Request(BASE + path, method=method, headers=H,
                                 data=json.dumps(body).encode() if body is not None else None)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read()
            return r.status, (json.loads(raw) if raw else None)
    except urllib.error.HTTPError as e:
        return e.code, None
    except (urllib.error.URLError, OSError):
        return 0, None


def pretend(args):
    """What a command run in a pretend sandbox prints, and its exit code."""
    cmd = args[-1] if args[:2] == ["sh", "-c"] else " ".join(args)
    # The console's terminal wraps each line: `cd <dir>` first, then the
    # line, then the directory it ended in after a \x1e. Answer the line,
    # and say where a `cd` in it would have left it.
    if "\n__rc=$?\n" in cmd:
        head, _, _ = cmd.partition("\n__rc=$?\n")
        lines = head.split("\n")
        here = "/root"
        if lines[0].startswith("cd '") and "2>/dev/null" in lines[0]:
            here = lines[0][4:lines[0].index("'", 4)]
            lines = lines[1:]
        line = "\n".join(lines).strip()
        if line.startswith("cd"):
            to = (line[2:].strip() or "/root").strip("'\"")
            here = to if to.startswith("/") else (here.rstrip("/") + "/" + to)
            return "\x1e" + here, "", 0
        out, err, code = pretend(["sh", "-c", line])
        return out + "\x1e" + here, err, code
    if cmd == "pwd":
        return "/root\n", "", 0
    if cmd in ("true", ""):
        return "", "", 0
    if cmd.startswith("false") or "exit 1" in cmd or "fail" in cmd:
        return "", f"sh: {cmd.split()[0]}: failed\n", 1
    if cmd.startswith("echo "):
        return cmd[5:].strip("'\"") + "\n", "", 0
    if cmd.startswith(("pip install", "pip3 install")):
        pkgs = [p for p in cmd.split()[2:] if not p.startswith("-")]
        out = "".join(f"Collecting {p}\n  Downloading {p}-1.0-py3-none-any.whl (1.2 MB)\n" for p in pkgs)
        return out + f"Successfully installed {' '.join(pkgs)}\n", "", 0
    return f"ran: {cmd}\n", "", 0


def answer(result):
    """Deliver an exec's or a branch's outcome, after it has 'run'."""
    time.sleep(random.uniform(0.3, 1.6))
    call("POST", "/v1/node/result", result)


# What each pretend node says its host is. Most match; edge-6 is a different
# machine and edge-5 runs the Firecracker backend, so the master puts each in
# a host group of its own, the way ADR 6 says real ones would be -- and the
# console's Groups page has hosts to compare.
EPYC_FLAGS = ("fpu vme de pse tsc msr pae mce cx8 apic sep mtrr pge mca cmov pat pse36 clflush mmx fxsr sse sse2 "
              "ht syscall nx mmxext fxsr_opt pdpe1gb rdtscp lm constant_tsc rep_good nopl nonstop_tsc cpuid "
              "extd_apicid aperfmperf pni pclmulqdq monitor ssse3 fma cx16 sse4_1 sse4_2 movbe popcnt aes xsave "
              "avx f16c rdrand lahf_lm cmp_legacy svm extapic cr8_legacy abm sse4a misalignsse 3dnowprefetch "
              "osvw ibs skinit wdt tce topoext perfctr_core perfctr_nb bpext perfctr_llc mwaitx cpb cat_l3 "
              "cdp_l3 hw_pstate ssbd mba ibrs ibpb stibp vmmcall fsgsbase bmi1 avx2 smep bmi2 cqm rdt_a rdseed "
              "adx smap clflushopt clwb sha_ni xsaveopt xsavec xgetbv1 xsaves cqm_llc cqm_occup_llc "
              "cqm_mbm_total cqm_mbm_local clzero irperf xsaveerptr rdpru wbnoinvd arat npt lbrv svm_lock "
              "nrip_save tsc_scale vmcb_clean flushbyasid decodeassists pausefilter pfthreshold avic "
              "v_vmsave_vmload vgif umip rdpid overflow_recov succor smca sme sev sev_es")
XEON_FLAGS = (EPYC_FLAGS.replace(" svm ", " vmx ").replace(" sha_ni", "").replace(" clzero", "")
              .replace(" sme sev sev_es", "") + " avx512f avx512dq avx512cd avx512bw avx512vl pku ospke")
EPYC = {"engine": "runsc version release-20261019.0", "kernel": "7.0.0-1003-aws",
        "cpu_vendor": "AuthenticAMD", "cpu_model": "AMD EPYC 7K62 48-Core Processor", "cpu_flags": EPYC_FLAGS}
XEON = {"engine": "runsc version release-20261019.0", "kernel": "5.15.0-126-generic",
        "cpu_vendor": "GenuineIntel", "cpu_model": "Intel(R) Xeon(R) Platinum 8375C CPU @ 2.90GHz",
        "cpu_flags": XEON_FLAGS}


def host_of(i, el=0.0):
    """(pool asked for, fingerprint, host) for pretend node i, `el` seconds
    in. edge-4 has runsc upgraded under it two minutes in, so the master has
    to move a registered node to another host group, and the overview's
    per-group history has a node that changed groups."""
    if i == NODES - 1 and NODES > 2:
        return "default", "fp-xeon-8375c", XEON
    if i == NODES - 2 and NODES > 3:
        return "default", "fp-fc112-7k62", dict(EPYC, engine="Firecracker v1.12.0")
    if i == 3 and el > 120:
        return "default", "fp-rel26-7k62", dict(EPYC, engine="runsc version release-20261026.0")
    return "default", "fp-epyc-7k62", EPYC


class Node(threading.Thread):
    def __init__(self, i):
        super().__init__(daemon=True)
        self.id = f"edge-{i + 1}"
        self.i = i
        self.held = {}          # id -> tier
        self.failing = {}       # id -> attempts
        self.rng = random.Random(i)
        self.mem = 1900 + 512 * (i % 3)

    def pressure(self):
        hot = sum(1 for t in self.held.values() if t == "hot")
        frozen = sum(1 for t in self.held.values() if t == "frozen")
        used = 380 + 15 * hot + 5 * frozen + 0.4 * len(self.held) + self.rng.uniform(-25, 25)
        free = max(40, int(self.mem - used))
        psi = round(max(0.0, (650 - free) / 45) + self.rng.uniform(0, 0.4), 1)
        return free, psi

    def run(self):
        while time.time() - START < SECS:
            el = time.time() - START
            # One node in six goes quiet for a minute every eight.
            if self.i == 3 and int(el) % 480 > 420:
                time.sleep(2)
                continue
            traffic = []
            cold = [s for s, t in self.held.items() if t in ("frozen", "snapshot")]
            for s in self.rng.sample(cold, min(len(cold), self.rng.choice([0, 0, 0, 1, 2]))):
                self.held[s] = "hot"     # the gateway wakes it before the master hears
                traffic.append(s)
            free, psi = self.pressure()
            body = {
                "node_id": self.id,
                "observed": [{"id": s, "tier": t, "serving": t != "detached",
                              "ip": "10.86.0.2", "net_index": None}
                             for s, t in self.held.items()],
                "free_memory_mib": free,
                "pressure_some_avg10": psi,
                "hold_ms": 0,
                "failures": [{"id": s, "error": err, "attempts": n}
                             for s, (n, err) in self.failing.items()],
                "traffic": traffic,
                "features": ["reclaim-frozen", "swap:zswap"] + (["snapshot-compression"] if self.i % 2 else []),
                "host": host_of(self.i, el)[2],
                "fingerprint": host_of(self.i, el)[1],
            }
            st, resp = call("POST", "/v1/node/poll", body)
            for c in (resp or {}).get("commands", []):
                op, sid = c.get("op"), c.get("id")
                if op == "create":
                    if sid in self.failing:
                        n, err = self.failing[sid]
                        self.failing[sid] = (n + 1, err)
                    elif self.rng.random() < 0.012:
                        self.failing[sid] = (1, self.rng.choice([
                            "runsc create: exited with status 128: cannot create gofer process",
                            "node held it back: 471 MiB free, below its own 512 MiB floor",
                            "setting up the network: ip netns add: file exists"]))
                    else:
                        self.held[sid] = "hot"
                elif op == "set_tier" and sid in self.held:
                    self.held[sid] = c["tier"]
                elif op == "exec":
                    out, err, code = pretend(c.get("args", [])) if self.held.get(sid) == "hot" \
                        else ("", f"{sid} is not running here", -1)
                    threading.Thread(target=answer, daemon=True, args=({
                        "result_id": c["exec_id"], "stdout": out, "stderr": err, "exit_code": code},)).start()
                elif op == "branch":
                    threading.Thread(target=answer, daemon=True, args=({
                        "result_id": c["result_id"], "exit_code": 0,
                        "size_bytes": self.rng.randint(20, 240) * 2 ** 20},)).start()
                elif op == "destroy":
                    self.held.pop(sid, None)
                    self.failing.pop(sid, None)
            time.sleep(1.5)


def client():
    rng = random.Random(42)
    mine = []
    while time.time() - START < SECS:
        el = time.time() - START
        # A slow tide with bursts on top: creates rise and fall over ten
        # minutes, and every few minutes a batch job asks for a lot at once.
        rate = 2.5 + 2.5 * math.sin(el / 95) + (14 if int(el) % 300 < 8 else 0)
        for _ in range(max(0, int(rng.gauss(rate, 1)))):
            st, b = call("POST", "/v1/sandboxes", {"template": rng.choice(["python3.12", "python3.12", "node22"])})
            if st == 201 and b:
                mine.append(b["id"])
        # Deletes trail creates so the fleet breathes instead of only filling.
        keep = 900 + 300 * math.sin(el / 400)
        drop = rng.randint(0, 3) + (rng.randint(2, 6) if len(mine) > keep else 0)
        for _ in range(min(len(mine), drop)):
            call("DELETE", "/v1/sandboxes/" + mine.pop(rng.randrange(len(mine))))
        # Some callers come back to a sandbox they already have.
        for sid in rng.sample(mine, min(len(mine), 3)):
            call("POST", f"/v1/sandboxes/{sid}/touch")
        time.sleep(1)


def main():
    for i in range(NODES):
        nid = f"edge-{i + 1}"
        pool, fp, host = host_of(i)
        _, tok = call("POST", "/v1/node-tokens", {"pool": pool})
        call("POST", "/v1/node/register", {"token": tok["token"], "node_id": nid,
                                           "fingerprint": fp, "max_sandboxes": 0, "host": host})
    for name, args in [("python3.12", ["/usr/bin/python3", "/app/server.py"]),
                       ("node22", ["/usr/bin/node", "/app/server.js"])]:
        call("POST", "/v1/templates", {"name": name, "rootfs": "rootfs",
                                       "spec": {"args": args, "memory_max_mib": 256}})
    for i in range(NODES):
        Node(i).start()
    client()


if __name__ == "__main__":
    main()
