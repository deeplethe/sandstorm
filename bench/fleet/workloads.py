#!/usr/bin/env python3
"""Workloads that look more like agents than `loadtest.py`'s sleepers.

`loadtest.py` measures the lifecycle: how many sandboxes, how fast they are
placed. It never sends a sandbox a request, so it cannot say what a caller
waits for. These do. Every request goes where a real one would -- straight
to the node's gateway with the sandbox's own token, never through the
master -- and the gateway wakes whatever it finds cold.

    workloads.py sleep  --template app64 --count 84
    workloads.py herd   --template app64 --sizes 6,24,60,120,240
    workloads.py mixed  --template app64 --count 1200 --rates 5,10,20,40 --phase-secs 180
    workloads.py mixed  ... --retries 3      # callers that wait out retry-after
    workloads.py sizes  --templates idle,app16,app64,app256
    workloads.py fill   --template web --count 3400 --rate 3.4 --tokens t.jsonl
    workloads.py probe  --count 100 --tokens t.jsonl   # call a sample of them
    workloads.py watch  --duration 1200      # samples only, beside loadtest.py

Each writes one JSON line per event to --out, plus a `sample` line every
--sample-secs with the master's tier counts and each node's free memory,
so every run carries its own picture of the fleet beside its latencies.

Stdlib only. Imports `Client` from loadtest.py beside it.
"""

import argparse
import concurrent.futures as cf
import json
import random
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from loadtest import Client, parse_exposition  # noqa: E402


class Out:
    def __init__(self, path):
        self.f = open(path, "a", buffering=1)
        self.lock = threading.Lock()

    def write(self, kind, **row):
        row = {"t": round(time.time(), 3), "kind": kind, **row}
        with self.lock:
            self.f.write(json.dumps(row) + "\n")


class Fleet:
    def __init__(self, args):
        token = Path(args.token_file).read_text().strip()
        self.c = Client(args.master, token, args.ca, timeout=120)
        st, nodes, _ = self.c.call("GET", "/v1/nodes")
        if st != 200:
            sys.exit(f"cannot list nodes: {st} {nodes}")
        # The API does not say where a node is reachable from outside, and
        # the gateway is on the node, so the caller says: id=host,... With
        # --node, this run drives one node from that node itself: it places
        # every sandbox there and reaches the gateway over loopback, which
        # is how to measure when only the master's port is open between
        # machines.
        self.pin = args.node
        if self.pin:
            args.node_addrs = f"{self.pin}=127.0.0.1"
            nodes = [n for n in nodes if n["id"] == self.pin]
        addrs = dict(kv.split("=", 1) for kv in args.node_addrs.split(",") if kv)
        missing = [n["id"] for n in nodes if n["id"] not in addrs]
        if missing:
            sys.exit(f"--node-addrs has no address for {', '.join(missing)}")
        self.nodes = {n["id"]: addrs[n["id"]] for n in nodes}

    def create(self, template, port=8000):
        body = {"template": template, "port": port}
        if self.pin:
            body["node_id"] = self.pin
        st, v, took = self.c.call("POST", "/v1/sandboxes", body)
        if st not in (200, 201):
            return None, st, v, took
        return v, st, None, took

    def wait_ready(self, sb, deadline):
        """Poll until the sandbox has a node, an address and a gateway port.

        A node short of memory holds a create back and retries it. Meanwhile
        the idle policy counts the wait as idleness and demotes the sandbox
        before it ever ran, after which nothing starts it. Touching it while
        waiting keeps it wanted hot -- what a caller who is waiting means.
        """
        touched = time.time()
        while time.time() < deadline:
            st, v, _ = self.c.call("GET", f"/v1/sandboxes/{sb['id']}")
            if st == 200 and v.get("gateway_port") and v.get("node_id") and v.get("ip"):
                sb.update(node_id=v["node_id"], gateway_port=v["gateway_port"])
                return True
            if time.time() - touched > 5:
                if st == 200 and v.get("last_error"):
                    # Held back so often it is parked; ask again.
                    self.c.call("POST", f"/v1/sandboxes/{sb['id']}/retry")
                self.c.call("POST", f"/v1/sandboxes/{sb['id']}/touch")
                touched = time.time()
            time.sleep(0.25)
        return False

    def url(self, sb):
        return f"http://{self.nodes[sb['node_id']]}:{sb['gateway_port']}/"

    def delete(self, sb):
        self.c.call("DELETE", f"/v1/sandboxes/{sb['id']}")

    def sample(self):
        st, text, _ = self.c.call("GET", "/metrics", accept_json=False)
        tiers, free, per_node = {}, {}, {}
        if st == 200:
            m = parse_exposition(text)
            for labels, v in m.get("sandstorm_sandboxes", []):
                if "tier" in labels:
                    tiers[labels["tier"]] = tiers.get(labels["tier"], 0) + v
            for labels, v in m.get("sandstorm_node_free_memory_mib", []):
                free[labels.get("node", "?")] = v
            for labels, v in m.get("sandstorm_node_sandboxes", []):
                per_node[labels.get("node", "?")] = per_node.get(labels.get("node", "?"), 0) + v
        return {"tiers": tiers, "free_mib": free, "per_node": per_node}


def has_processes(sandbox):
    """Whether this sandbox still has anything running on this node. Its
    cgroup empties when it has really reached the disk, under either backend;
    `runsc list` only knew of gVisor's, so under Firecracker every sandbox
    looked checkpointed at once."""
    try:
        with open(f"/sys/fs/cgroup/sandstorm/{sandbox}/cgroup.procs") as f:
            return bool(f.read().strip())
    except OSError:
        return False


def wait_on_disk(fleet, a, group, timeout=600):
    """Ask for the snapshot tier, then wait until the node has really done it.

    The master's tier is the one it wants; the node gets there later, one
    checkpoint at a time. Run on the node (--node), this looks for itself:
    a checkpointed sandbox has nothing left running.
    """
    with cf.ThreadPoolExecutor(16) as ex:
        list(ex.map(lambda s: fleet.c.call("POST", f"/v1/sandboxes/{s['id']}/tier", {"tier": "snapshot"}), group))
    ids = {s["id"] for s in group}
    t0 = time.time()
    while time.time() < t0 + timeout:
        if not any(has_processes(i) for i in ids):
            return time.time() - t0
        time.sleep(1)
    return None


def request(fleet, sb, timeout=60):
    """One GET through the gateway. Returns (status, seconds, woke_ms)."""
    st, secs, woke, _ = attempt(fleet, sb, timeout)
    return st, secs, woke


def attempt(fleet, sb, timeout):
    """One GET, also returning the retry-after a refusal came with."""
    req = urllib.request.Request(fleet.url(sb), headers={"x-sandstorm-sandbox-token": sb["token"]})
    t0 = time.perf_counter()
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            r.read()
            woke = r.headers.get("x-sandstorm-woke-ms")
            return r.status, time.perf_counter() - t0, float(woke) if woke else None, None
    except urllib.error.HTTPError as e:
        after = e.headers.get("retry-after")
        return e.code, time.perf_counter() - t0, None, float(after) if after else None
    except Exception:
        return 0, time.perf_counter() - t0, None, None


def patient(fleet, sb, retries, timeout=60):
    """A caller that does as a refusal asks: on a 503 with retry-after it
    waits that long (plus up to half again, so refused callers do not all
    come back at once) and tries again, at most `retries` times. Returns
    (status, seconds from the first try, woke_ms, statuses of every try)."""
    t0 = time.perf_counter()
    tries = []
    while True:
        st, _, woke, after = attempt(fleet, sb, timeout)
        tries.append(st)
        if st != 503 or after is None or len(tries) > retries:
            return st, time.perf_counter() - t0, woke, tries
        time.sleep(after * (1 + random.random() / 2))


def sampler(fleet, out, every, stop):
    while not stop.is_set():
        out.write("sample", **fleet.sample())
        stop.wait(every)


def make(fleet, out, template, count, workers=8, ready_secs=180):
    """Create `count`, wait until each answers one request. Returns them."""
    made, lock = [], threading.Lock()

    def one(_):
        sb, st, err, took = fleet.create(template)
        if sb is None:
            out.write("create_refused", template=template, status=st, error=str(err)[:200])
            return
        t0 = time.time()
        if not fleet.wait_ready(sb, t0 + ready_secs):
            out.write("never_ready", id=sb["id"], template=template)
            return
        # The sandbox is up before its app is listening: ask again until it
        # answers, so this measures create -> first useful response.
        nudged = time.time()
        while True:
            st, secs, woke = request(fleet, sb, timeout=ready_secs)
            if st == 200 or time.time() > t0 + ready_secs:
                break
            # A create the node held back too many times is parked until
            # someone asks again, even once memory is free. Ask again.
            if time.time() - nudged > 10:
                _, v, _ = fleet.c.call("GET", f"/v1/sandboxes/{sb['id']}")
                if isinstance(v, dict) and v.get("last_error"):
                    fleet.c.call("POST", f"/v1/sandboxes/{sb['id']}/retry")
                    fleet.c.call("POST", f"/v1/sandboxes/{sb['id']}/touch")
                    out.write("nudged", id=sb["id"], error=v["last_error"][:120], attempts=v.get("error_attempts"))
                nudged = time.time()
            time.sleep(0.25)
        out.write("first_request", id=sb["id"], node=sb["node_id"], template=template,
                  status=st, accepted_s=round(took, 4),
                  create_to_response_s=round(time.time() - t0 + took, 4), woke_ms=woke)
        if st == 200:
            with lock:
                made.append(sb)

    with cf.ThreadPoolExecutor(workers) as ex:
        list(ex.map(one, range(count)))
    return made


def cleanup(fleet, sbs):
    with cf.ThreadPoolExecutor(16) as ex:
        list(ex.map(fleet.delete, sbs))


# --- sleep: how long a request waits, by how long the sandbox slept -------

def cmd_sleep(fleet, out, a):
    ages = [float(x) for x in a.ages.split(",")]
    sbs = make(fleet, out, a.template, a.count)
    random.shuffle(sbs)
    # Each sandbox sleeps exactly once, for one of the ages, so no request
    # disturbs another sandbox's idle clock.
    plan = [(sb, ages[i % len(ages)]) for i, sb in enumerate(sbs)]
    start = time.time()
    for sb, _ in plan:
        request(fleet, sb)  # the last touch before it sleeps
    marks = {sb["id"]: time.time() for sb, _ in plan}

    def fire(item):
        sb, age = item
        time.sleep(max(0.0, marks[sb["id"]] + age - time.time()))
        st, secs, woke = request(fleet, sb)
        out.write("wake", id=sb["id"], node=sb["node_id"], slept_s=age, status=st,
                  latency_s=round(secs, 4), woke_ms=woke)

    with cf.ThreadPoolExecutor(len(plan)) as ex:
        list(ex.map(fire, plan))
    out.write("done", secs=round(time.time() - start, 1))
    if not a.keep:
        cleanup(fleet, sbs)


# --- herd: K sandboxes woken at the same instant ---------------------------

def cmd_herd(fleet, out, a):
    sizes = [int(x) for x in a.sizes.split(",")]
    sbs = make(fleet, out, a.template, max(sizes), workers=a.create_workers, ready_secs=a.ready_secs)
    for k in sizes:
        group = random.sample(sbs, min(k, len(sbs)))
        took = wait_on_disk(fleet, a, group)
        out.write("checkpointed", size=len(group), secs=took)
        time.sleep(a.settle)
        barrier = threading.Barrier(len(group))

        def go(s):
            barrier.wait()
            st, secs, woke = request(fleet, s, timeout=300)
            return st, secs, woke, s["node_id"]

        t0 = time.time()
        with cf.ThreadPoolExecutor(len(group)) as ex:
            res = list(ex.map(go, group))
        out.write("herd", size=len(group), wall_s=round(time.time() - t0, 3),
                  ok=sum(r[0] == 200 for r in res),
                  latencies_s=sorted(round(r[1], 4) for r in res),
                  per_node={n: sum(r[3] == n for r in res) for n in fleet.nodes})
    if not a.keep:
        cleanup(fleet, sbs)


# --- mixed: a population with popular and forgotten sandboxes ---------------

def cmd_mixed(fleet, out, a):
    stop = threading.Event()
    threading.Thread(target=sampler, args=(fleet, out, a.sample_secs, stop), daemon=True).start()
    # A population bigger than memory comes up only as fast as older
    # sandboxes go to disk, so give it time.
    sbs = make(fleet, out, a.template, a.count, workers=a.create_workers, ready_secs=a.ready_secs)
    out.write("population", size=len(sbs))
    # Zipf popularity over a fixed random order: a few sandboxes get most of
    # the traffic and most get little -- the shape that makes tiering pay.
    random.shuffle(sbs)
    weights = [1.0 / (i + 1) ** a.zipf for i in range(len(sbs))]
    total = sum(weights)
    cum, acc = [], 0.0
    for w in weights:
        acc += w / total
        cum.append(acc)

    def pick():
        import bisect
        return sbs[min(bisect.bisect_left(cum, random.random()), len(sbs) - 1)]

    rank = {sb["id"]: i for i, sb in enumerate(sbs)}
    pool = cf.ThreadPoolExecutor(a.max_inflight)

    def fire(sb, rate):
        st, secs, woke, tries = patient(fleet, sb, a.retries)
        out.write("req", id=sb["id"], rank=rank[sb["id"]], node=sb["node_id"], rate=rate,
                  status=st, latency_s=round(secs, 4), woke_ms=woke, tries=tries)

    for rate in [float(x) for x in a.rates.split(",")]:
        out.write("phase", rate=rate)
        end = time.time() + a.phase_secs
        nxt = time.time()
        while nxt < end:
            # Open loop: arrivals do not wait for earlier answers, which is
            # what exposes a queue building up.
            nxt += random.expovariate(rate)
            time.sleep(max(0.0, nxt - time.time()))
            pool.submit(fire, pick(), rate)
    pool.shutdown(wait=True)
    stop.set()
    if not a.keep:
        cleanup(fleet, sbs)


# --- fill and probe: are the sandboxes a node holds still callable? --------

def cmd_fill(fleet, out, a):
    """Create sandboxes at a steady rate and keep what calling them takes.

    The ceiling runs count sandboxes created. `probe` afterwards is what
    says they are sandboxes and not just files, and it needs each one's
    token, which the master hands out once, at create. So they go to
    --tokens -- a file of secrets, kept out of the results.
    """
    keep = open(a.tokens, "a", buffering=1)
    made, nxt = 0, time.time()
    while made < a.count:
        sb, st, err, took = fleet.create(a.template)
        if sb is None:
            # Every node under its floor: the master says so rather than
            # placing. Come back shortly, as loadtest.py --through-refusals does.
            out.write("create_refused", status=st, error=str(err)[:160])
            time.sleep(1)
            nxt = time.time()
            continue
        keep.write(json.dumps({k: sb.get(k) for k in ("id", "token", "node_id", "gateway_port")}) + "\n")
        made += 1
        out.write("created", id=sb["id"], n=made, accepted_s=round(took, 4))
        nxt += 1.0 / a.rate
        time.sleep(max(0.0, nxt - time.time()))


def cmd_probe(fleet, out, a):
    """Call a random sample of the sandboxes `fill` made on this node.

    Run with the node full and the sample idle long enough to be on disk:
    each request then has to wake one, among thousands that are not.
    """
    mine = [json.loads(l) for l in open(a.tokens)]
    mine = [s for s in mine if s.get("node_id") in fleet.nodes and s.get("gateway_port")]
    pick = random.sample(mine, min(a.count, len(mine)))
    out.write("population", size=len(mine), sample=len(pick))

    def fire(sb):
        st, v, _ = fleet.c.call("GET", f"/v1/sandboxes/{sb['id']}")
        tier = v.get("tier") if st == 200 and isinstance(v, dict) else None
        st, secs, woke = request(fleet, sb)
        out.write("probe", id=sb["id"], node=sb["node_id"], tier=tier, status=st,
                  latency_s=round(secs, 4), woke_ms=woke)

    with cf.ThreadPoolExecutor(a.create_workers) as ex:
        list(ex.map(fire, pick))


# --- watch: only the fleet's own picture, for a run something else drives --

def cmd_watch(fleet, out, a):
    end = time.time() + a.duration
    while time.time() < end:
        out.write("sample", **fleet.sample())
        time.sleep(a.sample_secs)


# --- sizes: what a heavier sandbox costs ----------------------------------

def cmd_sizes(fleet, out, a):
    for tpl in a.templates.split(","):
        sbs = make(fleet, out, tpl, a.count)
        took = wait_on_disk(fleet, a, sbs)
        out.write("checkpointed", template=tpl, size=len(sbs), secs=took)
        time.sleep(a.settle)
        snap = fleet.sample()
        for s in sbs:
            st, secs, woke = request(fleet, s)
            out.write("restore", template=tpl, id=s["id"], node=s["node_id"], status=st,
                      latency_s=round(secs, 4), woke_ms=woke)
        st, text, _ = fleet.c.call("GET", "/metrics", accept_json=False)
        out.write("snapshot_bytes", template=tpl, count=len(sbs),
                  total=sum(v for _, v in parse_exposition(text).get("sandstorm_snapshot_bytes", [])) if st == 200 else None,
                  sample=snap)
        cleanup(fleet, sbs)
        time.sleep(a.settle)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cmd", choices=["sleep", "herd", "mixed", "sizes", "fill", "probe", "watch"])
    ap.add_argument("--master", required=True)
    ap.add_argument("--ca")
    ap.add_argument("--token-file", required=True)
    ap.add_argument("--node-addrs", default="", help="id=host,... where each node's gateway is reachable")
    ap.add_argument("--node", help="drive only this node, from this node")
    ap.add_argument("--out", required=True)
    ap.add_argument("--template", default="app64")
    ap.add_argument("--templates", default="idle,app16,app64,app256")
    ap.add_argument("--count", type=int, default=60)
    ap.add_argument("--ages", default="1,3,6,10,14,18,25,35,50,65,80,100,130,170")
    ap.add_argument("--sizes", default="6,24,60,120,240")
    ap.add_argument("--rates", default="5,10,20,40")
    ap.add_argument("--phase-secs", type=float, default=180)
    ap.add_argument("--zipf", type=float, default=1.1)
    ap.add_argument("--max-inflight", type=int, default=256)
    ap.add_argument("--retries", type=int, default=0,
                    help="mixed: retry a 503 that came with retry-after this many times, after waiting it out")
    ap.add_argument("--create-workers", type=int, default=8)
    ap.add_argument("--sample-secs", type=float, default=5)
    ap.add_argument("--settle", type=float, default=5)
    ap.add_argument("--duration", type=float, default=3600)
    ap.add_argument("--ready-secs", type=float, default=900)
    ap.add_argument("--keep", action="store_true")
    ap.add_argument("--rate", type=float, default=3.0, help="fill: creates per second")
    ap.add_argument("--tokens", help="fill: where to keep each sandbox's token; probe: where to read them")
    a = ap.parse_args()
    fleet = Fleet(a)
    out = Out(a.out)
    out.write("start", cmd=a.cmd, args=vars(a), nodes=fleet.nodes)
    {"sleep": cmd_sleep, "herd": cmd_herd, "mixed": cmd_mixed, "sizes": cmd_sizes,
     "fill": cmd_fill, "probe": cmd_probe, "watch": cmd_watch}[a.cmd](fleet, out, a)
    out.write("end")


if __name__ == "__main__":
    main()
