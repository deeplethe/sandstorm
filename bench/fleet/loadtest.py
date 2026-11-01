#!/usr/bin/env python3
"""Fleet load test for a v0.6 master.

What this is for
----------------

The v0.5 benchmarks (in the v0.5.3 tag) measured one host:
`bench-spawn-100.sh` spawned 100 sandboxes through the old single-host
CLI, and the `pause-window` and `chain-spawn` probes drove the HTTP API
one sandbox at a time. Neither
shape answers the questions a fleet raises -- how creates distribute
across nodes, where the master starts refusing, and whether the thing
in the way is a node or the master's own lock.

So this drives the **master API** (so it measures what a caller
actually experiences), **concurrently** (so queueing shows up), across
**however many nodes are registered** (so placement shows up), and it
reads `/metrics` either side of the run so the control plane's own view
sits next to the client's.

Three things to know before reading a number out of it
------------------------------------------------------

1. **A sandbox falls to `frozen` after 30 s idle** by default
   (`--idle-frozen-secs`). A run that creates a few hundred and then
   measures them is measuring restores, not steady state. `--touch`
   keeps them hot during the hold; without it the report says how many
   drifted so the number is not read as something else.

2. **Admission is up to `--hold-secs` stale and nothing is reserved.**
   A node reports free memory on its poll; a burst of creates arriving
   inside one hold window all see the same figure and all land on the
   same node. That overshoot is a real property of the scheduler, and
   at high `--concurrency` it is probably what you are measuring.

3. **A capacity refusal is a 503.** It means the fleet is full, not
   that the master is broken. The ramp stops there by default, because
   that is the capacity answer; `--through-refusals` keeps going.

Nothing here is destructive to a node: it creates sandboxes through the
API and deletes them again, and it will not start if the fleet already
holds sandboxes it did not make (`--allow-existing` overrides).
"""

from __future__ import annotations

import argparse
import json
import os
import ssl
import statistics
import sys
import threading
import time
import urllib.error
import urllib.request
from collections import Counter, defaultdict
from concurrent.futures import ThreadPoolExecutor
from typing import Any, Optional

# Series the report pulls out of /metrics. Everything else is carried in
# the JSON untouched.
COUNTERS = [
    "sandstorm_sandbox_creates_total",
    "sandstorm_sandbox_deletes_total",
    "sandstorm_demotions_total",
]
HISTOGRAMS = [
    "sandstorm_store_lock_wait_seconds",
    "sandstorm_store_lock_held_seconds",
    "sandstorm_demote_sweep_seconds",
    "sandstorm_http_request_duration_seconds",
]


class Client:
    """Minimal master client that times every call.

    Not the Python SDK: a load test wants the wall time of one request
    and nothing between it and the socket.
    """

    def __init__(self, base: str, token: str, ca: Optional[str], timeout: float):
        self.base = base.rstrip("/")
        self.token = token
        self.timeout = timeout
        self.ctx = (
            ssl.create_default_context(cafile=ca)
            if self.base.startswith("https://")
            else None
        )

    def call(
        self, method: str, path: str, body: Optional[dict] = None, accept_json=True
    ) -> tuple[int, Any, float]:
        """Returns (status, parsed-or-text, seconds). Never raises for a
        non-2xx: a refusal is data here, not an exception."""
        data = json.dumps(body).encode() if body is not None else None
        headers = {"Authorization": f"Bearer {self.token}"}
        if data is not None:
            headers["Content-Type"] = "application/json"
        req = urllib.request.Request(
            f"{self.base}{path}", data=data, method=method, headers=headers
        )
        started = time.perf_counter()
        try:
            with urllib.request.urlopen(req, timeout=self.timeout, context=self.ctx) as r:
                raw = r.read()
                took = time.perf_counter() - started
                if not accept_json:
                    return r.status, raw.decode("utf-8", "replace"), took
                return r.status, (json.loads(raw) if raw else None), took
        except urllib.error.HTTPError as e:
            took = time.perf_counter() - started
            raw = e.read()
            try:
                return e.code, json.loads(raw) if raw else {}, took
            except json.JSONDecodeError:
                return e.code, raw.decode("utf-8", "replace"), took
        except Exception as e:  # connection refused, TLS, timeout
            return 0, {"error": f"{type(e).__name__}: {e}"}, time.perf_counter() - started


# --- /metrics ---------------------------------------------------------


def parse_exposition(text: str) -> dict[str, list[tuple[dict, float]]]:
    """Prometheus text format -> {name: [(labels, value)]}.

    Enough of the format for what the master emits: no timestamps, no
    exemplars, no escaped label values beyond the ones it writes.
    """
    out: dict[str, list[tuple[dict, float]]] = defaultdict(list)
    for line in text.splitlines():
        if not line or line.startswith("#"):
            continue
        name, _, rest = line.partition("{")
        if rest:
            labelpart, _, value = rest.rpartition("}")
            labels = {}
            for pair in _split_labels(labelpart):
                k, _, v = pair.partition("=")
                labels[k.strip()] = v.strip().strip('"')
        else:
            name, _, value = line.rpartition(" ")
            labels = {}
        try:
            out[name.strip()].append((labels, float(value)))
        except ValueError:
            continue
    return out


def _split_labels(s: str) -> list[str]:
    """Split on commas that are not inside a quoted value."""
    parts, buf, inq = [], [], False
    for ch in s:
        if ch == '"':
            inq = not inq
        if ch == "," and not inq:
            parts.append("".join(buf))
            buf = []
        else:
            buf.append(ch)
    if buf:
        parts.append("".join(buf))
    return [p for p in parts if p.strip()]


def histogram_quantile(samples: list[tuple[dict, float]], q: float) -> Optional[float]:
    """The same linear interpolation Prometheus' histogram_quantile does,
    over one series' buckets."""
    buckets = sorted(
        (float("inf") if l["le"] == "+Inf" else float(l["le"]), v)
        for l, v in samples
        if "le" in l
    )
    if not buckets:
        return None
    total = buckets[-1][1]
    if total == 0:
        return 0.0
    target = q * total
    prev_edge, prev_count = 0.0, 0.0
    for edge, count in buckets:
        if count >= target:
            if edge == float("inf"):
                return prev_edge
            if count == prev_count:
                return edge
            # Assume a uniform spread inside the bucket, as Prometheus does.
            return prev_edge + (edge - prev_edge) * (target - prev_count) / (
                count - prev_count
            )
        prev_edge, prev_count = edge, count
    return buckets[-1][0]


def pick(samples: list[tuple[dict, float]], **want) -> float:
    for labels, v in samples:
        if all(labels.get(k) == str(w) for k, w in want.items()):
            return v
    return 0.0


# --- the run ----------------------------------------------------------


class Recorder:
    def __init__(self):
        self.lock = threading.Lock()
        self.ops: list[dict] = []

    def add(self, **row):
        with self.lock:
            self.ops.append(row)

    def of(self, kind: str) -> list[dict]:
        return [o for o in self.ops if o["op"] == kind]


def percentiles(values: list[float]) -> dict:
    if not values:
        return {}
    s = sorted(values)

    def at(q):
        if len(s) == 1:
            return s[0]
        i = q * (len(s) - 1)
        lo, hi = int(i), min(int(i) + 1, len(s) - 1)
        return s[lo] + (s[hi] - s[lo]) * (i - lo)

    return {
        "n": len(s),
        "min": s[0],
        "p50": at(0.50),
        "p90": at(0.90),
        "p99": at(0.99),
        "max": s[-1],
        "mean": statistics.fmean(s),
    }


def settle(c: Client, made: list[dict], secs: float) -> dict:
    """Did the sandboxes the master accepted actually come up?

    A create returns 201 as soon as the master has written desired
    state; the node does the work afterwards, on its next poll. So the
    ramp's "placed" count is what was ACCEPTED, and on its own it is not
    evidence of anything running. The first run of this harness against
    a real node reported "10 placed, 0 failed" while every one of them
    had failed on the node with

        creating netns: ["ip","netns","add",...] failed

    because the unit `install.sh` writes could not write /run. Nothing in
    the report said so. Hence this phase.

    Three states, from what the node reports back:
      up       the node gave it an address
      pending  accepted, no address yet, no error yet
      failed   the node could not start it and said why
    """
    if not made:
        return {}
    time.sleep(secs)
    st, boxes, _ = c.call("GET", "/v1/sandboxes")
    if st != 200 or not isinstance(boxes, list):
        return {"error": f"could not read sandboxes back ({st})"}
    mine = {m["id"] for m in made}
    rows = [b for b in boxes if b["id"] in mine]
    failed = [b for b in rows if b.get("last_error")]
    up = [b for b in rows if not b.get("last_error") and b.get("ip")]
    pending = [b for b in rows if not b.get("last_error") and not b.get("ip")]
    return {
        "after_secs": secs,
        "up": len(up),
        "pending": len(pending),
        "failed": len(failed),
        "gone": len(mine) - len(rows),
        "errors": Counter(b["last_error"][:160] for b in failed).most_common(3),
    }


def fleet_snapshot(c: Client) -> dict:
    st, nodes, _ = c.call("GET", "/v1/nodes")
    st2, boxes, _ = c.call("GET", "/v1/sandboxes")
    return {
        "nodes": nodes if st == 200 else [],
        "sandboxes": boxes if st2 == 200 else [],
    }


def scrape(c: Client) -> dict:
    st, text, _ = c.call("GET", "/metrics", accept_json=False)
    if st != 200:
        return {}
    return parse_exposition(text)


def run(args) -> dict:
    token = open(args.token_file).read().strip() if args.token_file else args.token
    c = Client(args.master, token, args.ca, args.timeout)

    # --- preflight ----------------------------------------------------
    st, detail, _ = c.call("GET", "/healthz", accept_json=False)
    if st != 200:
        # Status 0 is a transport failure, and by far the most common one
        # is the master's self-signed CA with no --ca to check it against.
        # Saying "not answering" would send someone looking at the network.
        if st == 0:
            why = detail.get("error", "") if isinstance(detail, dict) else str(detail)
            hint = ""
            if "CERTIFICATE_VERIFY" in why or "SSLCertVerification" in why:
                hint = (
                    f"\n  The master signs its own certificate. Fetch its CA and pass it:"
                    f"\n    curl -k {args.master}/ca.pem -o ca.pem   # check the fingerprint"
                    f"\n    --ca ca.pem   (or SANDSTORM_CA=ca.pem)"
                )
            sys.exit(f"cannot reach {args.master}: {why}{hint}")
        sys.exit(f"master not answering at {args.master} (/healthz -> {st})")
    snap = fleet_snapshot(c)
    nodes = snap["nodes"]
    up = [n for n in nodes if n.get("connected")]
    if not up:
        sys.exit("no connected node; nothing would be placed")
    pre_existing = snap["sandboxes"]
    if pre_existing and not args.allow_existing:
        sys.exit(
            f"the fleet already holds {len(pre_existing)} sandbox(es) this run did not "
            "make. Re-run with --allow-existing if that is intended; otherwise the "
            "report's placement and teardown numbers would be about someone else's work."
        )
    print(f"master   {args.master}")
    print(f"nodes    {len(up)} connected of {len(nodes)}")
    by_pool: dict[str, list] = defaultdict(list)
    for n in up:
        by_pool[n["pool"]].append(n)
    for pool in sorted(by_pool):
        print(f"  pool {pool}")
        for n in by_pool[pool]:
            why = n.get("not_admitting")
            print(
                f"    {n['id']:<16} free={n['free_mib']} MiB "
                f"psi={n['psi_some_avg10']:.1f} cap={n['max_sandboxes'] or 'none'}"
                + (f"  NOT ADMITTING: {why}" if why else "")
            )
    targets = by_pool.get(args.pool, [])
    if not targets:
        sys.exit(
            f"no connected node in pool {args.pool!r}. Pools present: "
            f"{', '.join(sorted(by_pool))}. Pass --pool to pick one."
        )
    if len(by_pool) > 1:
        # Not a misconfiguration: a node whose host fingerprint differs
        # from the pool's joins a DERIVED pool (`default#<fp8>`), because
        # a runsc checkpoint does not restore across differing hosts. So
        # a fleet of mixed machines splits itself up, and a run against
        # one pool uses only part of it. Said loudly because the result
        # otherwise just looks like a scheduler that will not spread.
        print(
            f"\n  NOTE  {len(by_pool)} pools. This run targets {args.pool!r}, which is "
            f"{len(targets)} of {len(up)} connected nodes."
        )
        print(
            "        Pools split by host fingerprint -- differing kernel/CPU means a "
            "checkpoint\n        cannot restore there. To load one pool at a time, run "
            "once per pool."
        )
    before = scrape(c)
    if not before:
        print("  (no /metrics on this master -- the control-plane view will be missing)")

    rec = Recorder()
    stop = threading.Event()
    body: dict = {"template": args.template} if args.template else {"snapshot_tag": args.tag}
    body["pool"] = args.pool

    # --- ramp ---------------------------------------------------------
    print(f"\nramp     {args.total} creates, {args.concurrency} at a time"
          + (f", {args.rate}/s" if args.rate else ""))
    t0 = time.perf_counter()
    wall0 = time.time()
    gate = threading.Semaphore(0)
    made: list[dict] = []
    made_lock = threading.Lock()

    def create(_i: int):
        if stop.is_set():
            return
        if args.rate:
            gate.acquire()
            if stop.is_set():
                return
        st, b, took = c.call("POST", "/v1/sandboxes", body)
        node = b.get("node_id") if isinstance(b, dict) else None
        rec.add(op="create", status=st, secs=took, node=node,
                error=None if st == 201 else _msg(b))
        if st == 201 and isinstance(b, dict):
            with made_lock:
                made.append(b)
        elif st == 503 and not args.through_refusals:
            # The fleet is full. That is the capacity answer, so stop
            # rather than spending the rest of the run on 503s.
            stop.set()

    def pacer():
        interval = 1.0 / args.rate
        for _ in range(args.total):
            if stop.is_set():
                break
            gate.release()
            time.sleep(interval)
        # Unblock anyone still waiting so the pool can drain.
        for _ in range(args.concurrency):
            gate.release()

    pacer_thread = None
    if args.rate:
        pacer_thread = threading.Thread(target=pacer, daemon=True)
        pacer_thread.start()
    with ThreadPoolExecutor(max_workers=args.concurrency) as ex:
        list(ex.map(create, range(args.total)))
    ramp_secs = time.perf_counter() - t0
    if pacer_thread:
        stop.set()
        pacer_thread.join(timeout=2)
    print(f"         {len(made)} accepted in {ramp_secs:.3f}s "
          f"({len(made)/ramp_secs:.1f}/s)")

    # Accepted is not running. Ask the nodes.
    settled = settle(c, made, args.settle_secs)
    if settled:
        if "error" in settled:
            print(f"settle   {settled['error']}")
        else:
            print(f"settle   after {settled['after_secs']:g}s: {settled['up']} up, "
                  f"{settled['pending']} pending, {settled['failed']} failed")

    # --- hold ---------------------------------------------------------
    samples = []
    if args.hold_secs:
        print(f"hold     {args.hold_secs}s" + ("  (touching)" if args.touch else ""))
        deadline = time.time() + args.hold_secs
        while time.time() < deadline:
            if args.touch:
                with ThreadPoolExecutor(max_workers=args.concurrency) as ex:
                    for sb in made:
                        ex.submit(
                            lambda s=sb: rec.add(
                                op="touch",
                                status=c.call(
                                    "POST", f"/v1/sandboxes/{s['id']}/touch"
                                )[0],
                                secs=0.0,
                                node=s.get("node_id"),
                                error=None,
                            )
                        )
            fs = fleet_snapshot(c)
            samples.append({
                "t": round(time.time() - wall0, 1),
                "tiers": dict(Counter(s["tier"] for s in fs["sandboxes"])),
                "nodes": {n["id"]: {"free_mib": n["free_mib"],
                                    "psi": n["psi_some_avg10"],
                                    "not_admitting": n.get("not_admitting")}
                          for n in fs["nodes"]},
            })
            time.sleep(min(5.0, max(0.0, deadline - time.time())))

    after_ramp = scrape(c)
    end_state = fleet_snapshot(c)

    # --- teardown -----------------------------------------------------
    if args.keep:
        print(f"\nkeep     {len(made)} sandboxes left running (--keep)")
    else:
        print(f"\nteardown {len(made)} deletes")
        def delete(sb):
            st, b, took = c.call("DELETE", f"/v1/sandboxes/{sb['id']}")
            rec.add(op="delete", status=st, secs=took, node=sb.get("node_id"),
                    error=None if st in (204, 404) else _msg(b))
        with ThreadPoolExecutor(max_workers=args.concurrency) as ex:
            list(ex.map(delete, made))
        left = fleet_snapshot(c)["sandboxes"]
        extra = len(left) - len(pre_existing)
        print(f"         {len(left)} sandbox(es) left"
              + ("" if extra <= 0 else f"  WARNING: {extra} of ours survived"))

    after = scrape(c)
    return report(args, rec, nodes, end_state, samples, before, after_ramp, after,
                  ramp_secs, len(made), settled)


def _msg(b) -> str:
    if isinstance(b, dict):
        return str(b.get("error") or b)[:200]
    return str(b)[:200]


def report(args, rec, nodes, end_state, samples, before, mid, after,
           ramp_secs, placed, settled) -> dict:
    creates = rec.of("create")
    ok = [o for o in creates if o["status"] == 201]
    refused = [o for o in creates if o["status"] == 503]
    other = [o for o in creates if o["status"] not in (201, 503)]
    deletes = rec.of("delete")

    placement = Counter(o["node"] for o in ok if o["node"])
    out = {
        "master": args.master,
        "args": {k: v for k, v in vars(args).items() if k != "token"},
        "ramp_secs": round(ramp_secs, 3),
        "creates": {
            "attempted": len(creates),
            "placed": len(ok),
            "refused": len(refused),
            "failed": len(other),
            "per_sec": round(len(ok) / ramp_secs, 2) if ramp_secs else None,
            "latency": percentiles([o["secs"] for o in ok]),
            "refusal_reasons": Counter(o["error"] for o in refused).most_common(5),
            "failures": Counter(f"{o['status']} {o['error']}" for o in other).most_common(5),
        },
        "settled": settled,
        "deletes": {"n": len(deletes), "latency": percentiles([o["secs"] for o in deletes])},
        "pool": args.pool,
        "placement": dict(placement),
        "end_tiers": dict(Counter(s["tier"] for s in end_state["sandboxes"])),
        "samples": samples,
        "control_plane": control_plane_delta(before, mid, after),
    }

    p = out["creates"]
    print("\n" + "=" * 62)
    print(f"creates   {p['placed']} accepted, {p['refused']} refused, {p['failed']} errored"
          f"   {p['per_sec']}/s")
    if settled and "error" not in settled:
        line = (f"started   {settled['up']} up, {settled['pending']} pending, "
                f"{settled['failed']} failed to start")
        # The headline must not read as success when nothing came up.
        print(line if not settled["failed"] else f"\033[31m{line}\033[0m")
        for e, n in settled["errors"]:
            print(f"  NOT RUNNING  {n}x  {e}")
        if settled["up"] == 0 and settled["pending"] == 0 and settled["failed"]:
            print("  Every sandbox the master accepted failed on its node. The latency")
            print("  numbers below are the control plane's only -- nothing ran.")
    if p["latency"]:
        L = p["latency"]
        print(f"          p50 {L['p50']*1000:.0f}ms   p90 {L['p90']*1000:.0f}ms   "
              f"p99 {L['p99']*1000:.0f}ms   max {L['max']*1000:.0f}ms")
    if placement:
        spread = ", ".join(f"{n}={k}" for n, k in placement.most_common())
        print(f"placement {spread}")
        if len(placement) > 1:
            lo, hi = min(placement.values()), max(placement.values())
            print(f"          spread {hi - lo} between the busiest and quietest node")
    for reason, n in p["refusal_reasons"]:
        print(f"refused   {n}x  {reason}")
    for f, n in p["failures"]:
        print(f"FAILED    {n}x  {f}")
    cp = out["control_plane"]
    if cp:
        print(f"store lock p50 {fmt_ms(cp['store_lock_wait_p50'])}  "
              f"p99 {fmt_ms(cp['store_lock_wait_p99'])}  "
              f"(held p99 {fmt_ms(cp['store_lock_held_p99'])})")
        print(f"master saw {cp['creates_placed']} placed, {cp['creates_refused']} refused,"
              f" {cp['demotions']} demotions")
        if cp["create_route_p99"] is not None:
            print(f"POST /v1/sandboxes server-side p99 {fmt_ms(cp['create_route_p99'])}")
    if out["end_tiers"]:
        print(f"tiers at end  {out['end_tiers']}")
    print("=" * 62)
    return out


def fmt_ms(v) -> str:
    return "-" if v is None else f"{v*1000:.1f}ms"


def control_plane_delta(before: dict, mid: dict, after: dict) -> dict:
    if not mid:
        return {}
    def counter(snap, name, **labels):
        return pick(snap.get(name, []), **labels)
    return {
        "creates_placed": counter(mid, "sandstorm_sandbox_creates_total", outcome="placed")
        - counter(before, "sandstorm_sandbox_creates_total", outcome="placed"),
        "creates_refused": counter(mid, "sandstorm_sandbox_creates_total", outcome="refused")
        - counter(before, "sandstorm_sandbox_creates_total", outcome="refused"),
        "demotions": sum(v for _, v in mid.get("sandstorm_demotions_total", []))
        - sum(v for _, v in before.get("sandstorm_demotions_total", [])),
        # Quantiles are over the process's whole life, not the window --
        # a counter difference is meaningful, a histogram's is not.
        "store_lock_wait_p50": histogram_quantile(
            after.get("sandstorm_store_lock_wait_seconds_bucket", []), 0.50),
        "store_lock_wait_p99": histogram_quantile(
            after.get("sandstorm_store_lock_wait_seconds_bucket", []), 0.99),
        "store_lock_held_p99": histogram_quantile(
            after.get("sandstorm_store_lock_held_seconds_bucket", []), 0.99),
        "create_route_p99": histogram_quantile(
            [(l, v) for l, v in after.get("sandstorm_http_request_duration_seconds_bucket", [])
             if l.get("route") == "/v1/sandboxes" and l.get("method") == "POST"], 0.99),
    }


def main():
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    ap.add_argument("--master", default=os.environ.get("SANDSTORM_URL", "https://127.0.0.1:8080"))
    ap.add_argument("--token-file", default=os.environ.get("SANDSTORM_TOKEN_FILE"))
    ap.add_argument("--token", default=os.environ.get("SANDSTORM_TOKEN", ""))
    ap.add_argument("--ca", default=os.environ.get("SANDSTORM_CA"),
                    help="PEM for the master's own CA; fetch with curl -k $MASTER/ca.pem")
    ap.add_argument("--pool", default="default",
                    help="pool to create in; a mixed fleet splits into default#<fp8> pools")
    ap.add_argument("--template", default="python")
    ap.add_argument("--tag", help="fork from this snapshot tag instead of a template")
    ap.add_argument("--total", type=int, default=100)
    ap.add_argument("--concurrency", type=int, default=16)
    ap.add_argument("--rate", type=float, help="creates per second; unset means as fast as the pool allows")
    ap.add_argument("--hold-secs", type=float, default=0,
                    help="keep the fleet up this long, sampling tiers and node health")
    ap.add_argument("--touch", action="store_true",
                    help="touch every sandbox during the hold so the 30s demotion does not fire")
    ap.add_argument("--through-refusals", action="store_true",
                    help="keep creating after the fleet starts refusing")
    ap.add_argument("--keep", action="store_true", help="do not delete what was created")
    ap.add_argument("--allow-existing", action="store_true",
                    help="run even though the fleet already holds sandboxes")
    ap.add_argument("--settle-secs", type=float, default=10.0,
                    help="wait this long after the ramp, then ask the nodes what actually "
                         "came up. 0 skips it, and the report then only says what the "
                         "master accepted")
    ap.add_argument("--timeout", type=float, default=60.0)
    ap.add_argument("--out", help="write the full result as JSON here")
    args = ap.parse_args()
    if args.template and args.tag:
        args.template = None
    if not (args.token or args.token_file):
        sys.exit("need --token-file or --token (the master's pki/api-token)")

    result = run(args)
    if args.out:
        with open(args.out, "w") as f:
            json.dump(result, f, indent=2, default=str)
        print(f"wrote {args.out}")


if __name__ == "__main__":
    main()
