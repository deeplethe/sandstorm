#!/usr/bin/env python3
"""Summarise workloads.py and baselines.py output, one table per kind of run.

    python3 bench/fleet/summary.py out/*.jsonl

Files are grouped by what they contain, not by name, so a directory of
every node's runs can be passed at once. Repeats are reported each on its
own line and then pooled, so a reader sees the spread before the average.
"""

import collections
import gzip
import json
import sys
from pathlib import Path


def q(xs, p):
    xs = sorted(xs)
    if not xs:
        return None
    k = (len(xs) - 1) * p
    f = int(k)
    c = min(f + 1, len(xs) - 1)
    return xs[f] + (xs[c] - xs[f]) * (k - f)


def ms(v):
    if v is None:
        return "-"
    return f"{v * 1000:.1f} ms" if v < 1 else f"{v:.2f} s"


def load(paths):
    runs = []
    for p in paths:
        rows = [json.loads(l) for l in (gzip.open(p, "rt") if str(p).endswith(".gz") else open(p)) if l.strip()]
        start = next((r for r in rows if r.get("kind") == "start"), {})
        runs.append((Path(p), start.get("cmd", "?"), rows))
    return runs


def mixed(runs):
    print("## mixed: requests by phase (per-node rate x nodes = fleet rate)")
    print(f"{'run':<28}{'rate':>6}{'requests':>10}{'refused':>10}{'p50':>11}{'p99':>11}")
    pooled = collections.defaultdict(list)
    for p, _, rows in runs:
        by = collections.defaultdict(list)
        for r in rows:
            if r.get("kind") == "req":
                by[r["rate"]].append(r)
                pooled[r["rate"]].append(r)
        for rate in sorted(by):
            v = by[rate]
            ok = [r["latency_s"] for r in v if r["status"] == 200]
            bad = sum(r["status"] != 200 for r in v)
            print(f"{p.stem[:27]:<28}{rate:>6g}{len(v):>10}{bad / len(v):>10.1%}{ms(q(ok, .5)):>11}{ms(q(ok, .99)):>11}")
    for rate in sorted(pooled):
        v = pooled[rate]
        ok = [r["latency_s"] for r in v if r["status"] == 200]
        bad = sum(r["status"] != 200 for r in v)
        print(f"{'ALL':<28}{rate:>6g}{len(v):>10}{bad / len(v):>10.1%}{ms(q(ok, .5)):>11}{ms(q(ok, .99)):>11}")
    print()


def probe(runs):
    print("## probe: a random sample of a full node's sandboxes, each called once")
    v = [r for _, _, rows in runs for r in rows if r.get("kind") == "probe"]
    if not v:
        return
    ok = [r["latency_s"] for r in v if r["status"] == 200]
    tiers = collections.Counter(r.get("tier") for r in v)
    print(f"called {len(v)}, answered {len(ok)} ({len(ok) / len(v):.1%}); tiers before: {dict(tiers)}")
    print(f"latency p50 {ms(q(ok, .5))}, p90 {ms(q(ok, .9))}, p99 {ms(q(ok, .99))}, max {ms(max(ok) if ok else None)}")
    print()


def baselines(runs):
    print("## baselines: kept in memory until MemAvailable < 512 MiB")
    print(f"{'runtime':<10}{'app':<8}{'instances':>10}{'start p50':>12}{'answer p50':>12}")
    for _, _, rows in runs:
        starts = collections.defaultdict(list)
        for r in rows:
            if r.get("kind") == "started":
                starts[(r["runtime"], r["app"])].append(r["start_to_answer_s"])
        for r in rows:
            if r.get("kind") == "density":
                k = (r["runtime"], r["app"])
                print(f"{r['runtime']:<10}{r['app']:<8}{r['instances']:>10}{ms(q(starts[k], .5)):>12}"
                      f"{(str(r['answer_p50_ms']) + ' ms') if r['answer_p50_ms'] is not None else '-':>12}")
        ck = [r for r in rows if r.get("kind") == "criu"]
        if ck:
            print(f"CRIU (podman, app64, {len(ck)}): checkpoint p50 {ms(q([r['checkpoint_s'] for r in ck], .5))}, "
                  f"restore to answer p50 {ms(q([r['restore_to_answer_s'] for r in ck if r['restore_to_answer_s']], .5))}")
    print()


def main():
    runs = load(sys.argv[1:])
    kinds = collections.defaultdict(list)
    for run in runs:
        kinds[run[1]].append(run)
    for kind, fn in (("mixed", mixed), ("probe", probe), ("baselines", baselines)):
        if kinds.get(kind):
            fn(kinds[kind])
    other = sorted(k for k in kinds if k not in ("mixed", "probe", "baselines"))
    if other:
        print("not summarised here:", ", ".join(other))


if __name__ == "__main__":
    main()
