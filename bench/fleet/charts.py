#!/usr/bin/env python3
"""Draw the fleet charts from a results directory.

    python3 bench/fleet/charts.py bench/fleet/results/2026-10-26 docs/assets/charts

Every chart is written twice, `-light.svg` and `-dark.svg`, for a
`<picture>` to choose between. Stdlib only, like loadtest.py: the SVG is
written by hand, so redrawing needs nothing installed. A chart whose data
is missing from the directory is skipped, not faked.
"""

import csv
import gzip
import itertools
import json
import math
import sys
from pathlib import Path

FONT = 'system-ui, -apple-system, "Segoe UI", Helvetica, Arial, sans-serif'

# Categorical slots 1-3 of the dataviz reference palette, stepped for each
# surface; validated adjacent-pair CVD dE >= 9.2 in both modes.
THEMES = {
    "light": {
        "surface": "#fcfcfb", "ink": "#0b0b0b", "ink2": "#52514e", "muted": "#898781",
        "grid": "#ecebe6", "axis": "#c3c2b7", "band": "#f3f2ee",
        "series": ["#2a78d6", "#eb6834", "#1baf7a"],
    },
    "dark": {
        "surface": "#1a1a19", "ink": "#f0efec", "ink2": "#c3c2b7", "muted": "#898781",
        "grid": "#262624", "axis": "#383835", "band": "#222220",
        "series": ["#3987e5", "#d95926", "#199e70"],
    },
}

W = 800
_ids = itertools.count()


# --- drawing primitives -----------------------------------------------------

class Chart:
    """A titled plot area with one x scale and one y scale."""

    def __init__(self, theme, title, subtitle, h=400, legend=(), top_pad=0, left=60, right=32, bottom=44, w=W, compact=False, title_left=None):
        """`compact` is a panel in a figure: smaller type, a shorter header."""
        self.t = THEMES[theme]
        self.w, self.h = w, h
        self.left, self.right, self.bottom = left, right, bottom
        self.fs = 11 if compact else 12
        self.compact = compact
        ty, sy, ly, self.top = (28, 47, 68, 80) if compact else (40, 64, 92, 96)
        cw = 6.3 if compact else 7
        self.top += (20 if compact else 22) if legend else 0
        self.top += top_pad
        self.out = [f'<rect width="{w}" height="{h}" rx="12" fill="{self.t["surface"]}"/>']
        tx = self.left if title_left is None else title_left
        self.text(tx, ty, title, size=14 if compact else 18, weight=600, colour=self.t["ink"])
        self.text(tx, sy, subtitle, size=self.fs if compact else 13, colour=self.t["ink2"])
        x = tx
        for colour, label, kind in legend:
            if x + 30 + cw * len(label) > w - self.right:
                x, ly = tx, ly + 20
                self.top += 20
            if kind == "line":
                self.out.append(f'<line x1="{x}" x2="{x + 16}" y1="{ly}" y2="{ly}" stroke="{colour}" stroke-width="2.5" stroke-linecap="round"/>')
                x += 22
            else:
                self.out.append(f'<circle cx="{x + 5}" cy="{ly}" r="4.5" fill="{colour}"/>')
                x += 16
            self.text(x, ly + 4, label, size=self.fs, colour=self.t["ink2"])
            x += cw * len(label) + 22

    @property
    def plot(self):
        return self.left, self.top, self.w - self.right, self.h - self.bottom

    def text(self, x, y, s, size=None, colour=None, anchor="start", weight=400):
        size = size or self.fs
        self.out.append(
            f'<text x="{x:.1f}" y="{y:.1f}" font-size="{size}" font-weight="{weight}" '
            f'fill="{colour or self.t["muted"]}" text-anchor="{anchor}">{s}</text>'
        )

    def xscale(self, lo, hi, log=False):
        x0, _, x1, _ = self.plot
        if log:
            a, b = math.log10(lo), math.log10(hi)
            self.sx = lambda v: x0 + (x1 - x0) * (math.log10(max(v, lo)) - a) / (b - a)
        else:
            self.sx = lambda v: x0 + (x1 - x0) * (v - lo) / (hi - lo)

    def yscale(self, lo, hi, log=False):
        _, y0, _, y1 = self.plot
        if log:
            a, b = math.log10(lo), math.log10(hi)
            self.sy = lambda v: y1 - (y1 - y0) * (math.log10(min(max(v, lo), hi)) - a) / (b - a)
        else:
            self.sy = lambda v: y1 - (y1 - y0) * (v - lo) / (hi - lo)

    def ygrid(self, ticks, fmt):
        x0, _, x1, _ = self.plot
        for i, v in enumerate(ticks):
            y = self.sy(v)
            self.out.append(f'<line x1="{x0}" x2="{x1}" y1="{y:.1f}" y2="{y:.1f}" stroke="{self.t["axis"] if i == 0 else self.t["grid"]}"/>')
            self.text(x0 - (8 if self.compact else 10), y + 4, fmt(v), anchor="end")

    def xticks(self, ticks, fmt):
        _, _, _, y1 = self.plot
        for v in ticks:
            self.text(self.sx(v), y1 + (22 if self.compact else 24), fmt(v), anchor="middle")

    def band(self, xa, xb, label=None):
        _, y0, _, y1 = self.plot
        a, b = self.sx(xa), self.sx(xb)
        self.out.append(f'<rect x="{a:.1f}" y="{y0}" width="{b - a:.1f}" height="{y1 - y0}" fill="{self.t["band"]}"/>')
        if label:
            self.text((a + b) / 2, y0 + 14, label, size=11, anchor="middle")

    def vline(self, x, label=None):
        _, y0, _, y1 = self.plot
        X = self.sx(x)
        self.out.append(f'<line x1="{X:.1f}" x2="{X:.1f}" y1="{y0}" y2="{y1}" stroke="{self.t["axis"]}"/>')
        if label:
            self.text(X + 6, y0 + 14, label, size=11)

    def hline(self, y, label):
        x0, _, x1, _ = self.plot
        Y = self.sy(y)
        self.out.append(f'<line x1="{x0}" x2="{x1}" y1="{Y:.1f}" y2="{Y:.1f}" stroke="{self.t["ink2"]}" stroke-width="1.25" stroke-dasharray="4 4"/>')
        self.text(x1, Y - 8, label, colour=self.t["ink2"], anchor="end")

    def line(self, pts, colour, width=2.5, smooth=True, fill=False, dot_end=True):
        P = [(self.sx(x), self.sy(y)) for x, y in pts]
        d = monotone(P) if smooth and len(P) > 2 else "M" + " L".join(f"{x:.1f},{y:.1f}" for x, y in P)
        if fill:
            _, _, _, y1 = self.plot
            gid = f"g{next(_ids)}"
            self.out.append(
                f'<defs><linearGradient id="{gid}" x1="0" y1="0" x2="0" y2="1">'
                f'<stop offset="0" stop-color="{colour}" stop-opacity="0.18"/>'
                f'<stop offset="1" stop-color="{colour}" stop-opacity="0"/></linearGradient></defs>'
            )
            self.out.append(f'<path d="{d} L{P[-1][0]:.1f},{y1} L{P[0][0]:.1f},{y1} Z" fill="url(#{gid})"/>')
        self.out.append(f'<path d="{d}" fill="none" stroke="{colour}" stroke-width="{width}" stroke-linecap="round" stroke-linejoin="round"/>')
        if dot_end:
            self.dot(*pts[-1], colour)
        return P

    def dot(self, x, y, colour, r=4.5, opacity=1.0, ring=True):
        ring_attr = f' stroke="{self.t["surface"]}" stroke-width="2"' if ring else ""
        self.out.append(f'<circle cx="{self.sx(x):.1f}" cy="{self.sy(y):.1f}" r="{r}" fill="{colour}" fill-opacity="{opacity}"{ring_attr}/>')

    def svg(self, label):
        return svg(self.w, self.h, label, "\n".join(self.out))


def svg(w, h, label, body):
    return (
        f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{w}" height="{h}" '
        f"font-family='{FONT}' role=\"img\" aria-label=\"{label}\">\n{body}\n</svg>\n"
    )


def monotone(points):
    """An SVG path through `points` that never overshoots them.

    Fritsch-Carlson monotone cubic: smooth, but a value that holds flat is
    drawn flat, and the curve never passes a value the data did not have.
    """
    n = len(points)
    xs = [p[0] for p in points]
    ys = [p[1] for p in points]
    d = [(ys[i + 1] - ys[i]) / ((xs[i + 1] - xs[i]) or 1e-9) for i in range(n - 1)]
    m = [d[0]] + [0.0 if d[i - 1] * d[i] <= 0 else (d[i - 1] + d[i]) / 2 for i in range(1, n - 1)] + [d[-1]]
    for i in range(n - 1):
        if d[i] == 0:
            m[i] = m[i + 1] = 0.0
            continue
        a, b = m[i] / d[i], m[i + 1] / d[i]
        s = a * a + b * b
        if s > 9:
            t = 3 / s ** 0.5
            m[i], m[i + 1] = t * a * d[i], t * b * d[i]
    path = [f"M{xs[0]:.1f},{ys[0]:.1f}"]
    for i in range(n - 1):
        h = (xs[i + 1] - xs[i]) / 3
        path.append(
            f"C{xs[i] + h:.1f},{ys[i] + m[i] * h:.1f} "
            f"{xs[i + 1] - h:.1f},{ys[i + 1] - m[i + 1] * h:.1f} "
            f"{xs[i + 1]:.1f},{ys[i + 1]:.1f}"
        )
    return " ".join(path)


def quantile(xs, q):
    xs = sorted(xs)
    if not xs:
        return None
    k = (len(xs) - 1) * q
    f = math.floor(k)
    c = min(f + 1, len(xs) - 1)
    return xs[f] + (xs[c] - xs[f]) * (k - f)


def ms(v):
    return f"{v:g} ms" if v < 1000 else f"{v / 1000:g} s"


def jsonl(path):
    """Rows of a results file, gzipped or not; none if it is not there."""
    for p in (path, path.with_name(path.name + ".gz")):
        if p.exists():
            with (gzip.open(p, "rt") if p.suffix == ".gz" else open(p)) as f:
                return [json.loads(l) for l in f if l.strip()]
    return []


# Where a round keeps each kind of run. A round that names them differently
# says so in its own builds.json; 2026-10-28 is the default.
BUILDS = {"node": "v6", "traffic": "v12", "fills": "v7/fills", "walls": "v7/walls", "baselines": "baselines"}


def builds(src):
    p = src / "builds.json"
    return {**BUILDS, **(json.load(open(p)) if p.exists() else {})}


def runs(src, name, later=None, build=None):
    """Every row of one kind of run.

    Round 3 kept one file per kind (r3/sleep.jsonl). Later rounds keep each
    node's runs apart (v6/<node>/sleep-1.jsonl.gz, ...): each row is tagged
    with the run it came from, so times are never compared across runs.
    `build` picks which build's runs: the traffic runs were taken again on v12.
    """
    rows = jsonl(src / "r3" / f"{name}.jsonl")
    if rows:
        return rows
    out = []
    build = builds(src)[build or "node"]
    stem = later or name
    files = sorted(src.glob(f"{build}/*/{stem}-*.jsonl.gz")) + sorted(src.glob(f"{build}/*/{stem}.jsonl.gz"))
    for i, f in enumerate(files):
        out += [{**r, "_run": i} for r in jsonl(f)]
    return out


def walls(src, app):
    """Sandboxes per node with every one kept in memory, for one template."""
    p = src / "r3" / f"wall_{app}.json"
    if p.exists():
        return json.load(open(p))["settled"]["up"] / 6
    ups = [json.load(open(f))["settled"]["up"] / 6 for f in sorted(src.glob(f"{builds(src)['walls']}/wall_{app}-*.json"))]
    return sum(ups) / len(ups) if ups else None


def baseline(src, runtime, app):
    """Median instances per node a runtime kept in memory (baselines.py)."""
    n = [r["instances"] for f in sorted((src / builds(src)["baselines"]).glob("*-baselines-*.jsonl")) for r in jsonl(f)
         if r.get("kind") == "density" and r["runtime"] == runtime and r["app"] == app]
    return quantile(n, 0.5) if n else None


# --- the charts ---------------------------------------------------------------

def running(src, theme, w, h):
    """The running count over a ramp: before #351, after it, and after the FD fix."""
    t = THEMES[theme]

    def load(name):
        p = src / name
        return [(int(r["seconds"]), int(r["running"])) for r in csv.DictReader(open(p))] if p.exists() else []

    runs = [
        (load("running-r3b.csv"), t["series"][0], "snapshots release their netns, open-file limit raised"),
        (load("running.csv"), t["series"][1], "before: each snapshot kept its netns"),
        (load("running-r3a.csv"), t["series"][2], "netns released, open-file limit left at 1024"),
    ]
    runs = [r for r in runs if r[0]]
    if not runs:
        return None
    peak = max(max(v for _, v in pts) for pts, _, _ in runs)
    ymax = math.ceil(peak * 1.12 / 4000) * 4000
    xmax = 19 * 60
    c = Chart(theme, f"{peak:,} sandboxes running on six 2-core, 2 GB machines",
              "Idle Python sandboxes created at 15-20/s. The dashed line is what the same machines hold kept in memory.",
              legend=[(colour, label, "line") for _, colour, label in runs], w=w, h=h)
    # Three legend entries do not fit on one row; give the third its own.
    c.xscale(0, xmax)
    c.yscale(0, ymax)
    c.ygrid(range(0, ymax + 1, 4000), lambda v: "0" if v == 0 else f"{v // 1000}k")
    c.xticks(range(0, xmax + 1, 180), lambda s: f"{s // 60} min")
    c.hline(431, "431, every sandbox in memory")
    for pts, colour, _ in reversed(runs):
        pts = [(x, y) for x, y in pts if x <= xmax]
        c.line(pts, colour, width=2.5 if colour == t["series"][0] else 2, fill=colour == t["series"][0])
        x, y = pts[-1]
        below = colour == t["series"][2]
        c.text(c.sx(x) - 8, c.sy(y) + (20 if below else -10), f"{max(v for _, v in pts):,}", size=12, colour=t["ink"], anchor="end", weight=600)
    return c


def held(src, theme, w, h, compact=False):
    """A fill's count of sandboxes over time, from the watcher's samples.

    The dashed line is what the same six machines hold with every sandbox
    kept in memory: Sandstorm's own wall, and Docker with gVisor's.
    """
    fills = src / builds(src)["fills"]
    first = sorted(fills.glob("fill-*-samples.jsonl*"))
    first = [f.with_name(f.name.removesuffix(".gz")) for f in first]
    samples = [r for r in jsonl(first[0]) if r.get("kind") == "sample"] if first else []
    if not samples:
        return None
    t = THEMES[theme]
    t0 = samples[0]["t"]
    pts = [(r["t"] - t0, int(sum(r["tiers"].values()))) for r in samples]
    peak = max(v for _, v in pts)
    # stop a little after the fill reaches its peak
    end = next(x for x, v in pts if v >= peak) + 120
    pts = [(x, v) for x, v in pts if x <= end]
    awake = round(walls(src, "web") * 6)
    docker = round(baseline(src, "runsc", "web") * 6)
    probes = [r for f in sorted(fills.glob("probe-*/*.jsonl")) for r in jsonl(f) if r.get("kind") == "probe"]
    ok = sum(r["status"] == 200 for r in probes)
    if compact:
        # One series, so no legend: the title names it.
        c = Chart(theme, f"{peak:,} sandboxes on six 2 GB machines",
                  f"Created at 20/s; {ok:,} of {len(probes):,} sampled then answered.",
                  w=w, h=h, compact=True, left=44, right=20, bottom=36, title_left=20)
    else:
        c = Chart(theme, f"{peak:,} sandboxes on six 2-core, 2 GB machines",
                  f"Python HTTP servers created at 20/s. Afterwards {ok:,} of {len(probes):,} picked at random were called and answered.",
                  legend=[(t["series"][0], "sandboxes held", "line")], w=w, h=h)
    ymax = math.ceil(peak * 1.12 / 4000) * 4000
    c.xscale(0, end)
    c.yscale(0, ymax)
    c.ygrid(range(0, ymax + 1, 4000), lambda v: "0" if v == 0 else f"{v // 1000}k")
    c.xticks(range(0, int(end) + 1, 300), lambda s: f"{s // 60} min")
    c.hline(awake, f"{awake} kept in memory (Docker + gVisor: {docker})" if compact
            else f"{awake}, every sandbox kept in memory (Docker + gVisor: {docker})")
    c.line(pts, t["series"][0], width=2.5, fill=True)
    x, y = pts[-1]
    c.text(c.sx(x) - 8, c.sy(y) - 10, f"{peak:,}", size=12, colour=t["ink"], anchor="end", weight=600)
    return c


def sleep(src, theme, w, h):
    rows = [r for r in runs(src, "sleep") if r["kind"] == "wake" and r["status"] == 200]
    if not rows:
        return None
    t = THEMES[theme]
    frozen, snap = 15, 60
    c = Chart(theme, "How long a request waits",
              f"64 MiB heap. Frozen after {frozen} s idle, on disk after {snap} s.",
              legend=[(t["series"][0], "whole answer", "line"), (t["series"][1], "the wake itself", "line")],
              w=w, h=h, compact=True, left=52, right=20, bottom=36, title_left=20)
    xmax = max(r["slept_s"] for r in rows) * 1.05
    c.xscale(0, xmax)
    c.yscale(1, 1000, log=True)
    c.band(0, frozen, "hot")
    c.band(snap, xmax, "on disk")
    c.text((c.sx(frozen) + c.sx(snap)) / 2, c.plot[1] + 14, "frozen", anchor="middle")
    c.ygrid([1, 10, 100, 1000], ms)
    c.xticks([s for s in (0, 30, 60, 90, 120, 150, 180) if s <= xmax], lambda s: f"{s} s")
    ages = sorted({r["slept_s"] for r in rows})
    whole = [(a, quantile([r["latency_s"] * 1000 for r in rows if r["slept_s"] == a], 0.5)) for a in ages]
    # The gateway reports how long it spent waking (x-sandstorm-woke-ms); a
    # request that found its sandbox hot did not wait for a wake at all.
    woke = [(a, quantile([r["woke_ms"] or 0 for r in rows if r["slept_s"] == a], 0.5)) for a in ages]
    woke = [(a, v) for a, v in woke if v and v >= 1]
    c.line(whole, t["series"][0], smooth=False)
    c.line(woke, t["series"][1], smooth=False)
    for pts, colour, above in ((whole, t["ink"], True), (woke, t["ink2"], False)):
        a, v = pts[-1]
        c.text(c.sx(a) - 6, c.sy(v) + (-10 if above else 18), ms(round(v)), colour=colour, anchor="end", weight=600)
    hot = [v for a, v in whole if a < frozen]
    if hot:
        c.text(c.sx(frozen / 2), c.sy(quantile(hot, 0.5)) - 10, f"{quantile(hot, 0.5):.1f} ms", colour=t["ink"], anchor="middle", weight=600)
    fz = [v for a, v in woke if frozen < a < snap]
    if fz:
        c.text((c.sx(frozen) + c.sx(snap)) / 2, c.sy(quantile(fz, 0.5)) + 18, f"thaw {round(quantile(fz, 0.5))} ms", colour=t["ink2"], anchor="middle")
    return c


def herd(src, theme):
    rows = [r for r in runs(src, "herd") if r["kind"] == "herd"]
    if not rows:
        return None
    t = THEMES[theme]
    by = {}
    for r in rows:
        by.setdefault(r["size"] if r["size"] < 30 else 40, []).append(r)
    sizes = sorted(by)
    # Each node woke N of its own at once; the slowest answer on any node.
    wall = [(k, max(r["wall_s"] for r in by[k])) for k in sizes]
    p50 = [(k, quantile([x for r in by[k] for x in r["latencies_s"]], 0.5)) for k in sizes]
    woke = {k: (sum(r["ok"] for r in by[k]), sum(r["size"] for r in by[k])) for k in sizes}
    c = Chart(theme, "Waking many at once",
              f"Each of {len(by[sizes[0]])} nodes wakes N app64 snapshots at once; past what fits in memory, it refuses the rest.",
              legend=[(t["series"][0], "median answer", "line"), (t["series"][1], "slowest answer", "line")])
    ymax = math.ceil(max(v for _, v in wall) * 1.25)
    c.xscale(0, sizes[-1])
    c.yscale(0, ymax)
    c.ygrid(range(0, ymax + 1, 1), lambda v: f"{v} s")
    c.xticks(sizes, lambda k: f"{k}")
    c.text(W - c.right, c.h - 6, "woken at once, per node", anchor="end", size=11)
    c.line(wall, t["series"][1], smooth=False)
    c.line(p50, t["series"][0], smooth=False)
    for k, v in wall + p50:
        c.dot(k, v, t["series"][1] if (k, v) in wall else t["series"][0], r=4)
    for k, v in wall:
        ok, asked = woke[k]
        c.text(c.sx(k), c.sy(v) - 14, f"{ok / asked:.0%} woke", size=11, colour=t["ink2"], anchor="end" if k == sizes[-1] else "middle")
    return c.svg("Herd wake latency")


def mixed(src, theme, w, h):
    rows = [r for r in runs(src, "mixed", "mixed40", build="traffic") if r["kind"] == "req"]
    if not rows:
        return None
    t = THEMES[theme]
    # Nodes started their traffic a few seconds apart: time each from its own start.
    start = {}
    for r in rows:
        k = (r.get("_run"), r["node"])
        start[k] = min(start.get(k, 1e18), r["t"] - r["latency_s"])
    for r in rows:
        r["rel"] = r["t"] - r["latency_s"] - start[(r.get("_run"), r["node"])]
    nodes = len({r["node"] for r in rows})
    # The same measurement from round 3, before a wake could make room.
    before = {}
    if not (src / "r3").exists():
        for r in jsonl(src.parent / "2026-10-26" / "r3" / "mixed.jsonl"):
            if r["kind"] == "req":
                before.setdefault(r["rate"], []).append(r["status"] != 200)
    win = 15
    buckets = {}
    for r in rows:
        if r["status"] == 200:
            buckets.setdefault(int(r["rel"] // win), []).append(r["latency_s"] * 1000)
    keys = sorted(buckets)
    p50 = [((k + 0.5) * win, quantile(buckets[k], 0.5)) for k in keys if len(buckets[k]) >= 5]
    p99 = [((k + 0.5) * win, quantile(buckets[k], 0.99)) for k in keys if len(buckets[k]) >= 5]
    c = Chart(theme, "Under rising traffic",
              "40 per node, where about 12 fit awake. Zipf popularity.",
              legend=[(t["series"][0], "median", "line"), (t["series"][1], "p99", "line")],
              top_pad=16 if (src / "r3").exists() else 36, w=w, h=h, compact=True, left=52, right=20, bottom=36, title_left=20)
    xmax = (keys[-1] + 1) * win
    hi = 10 ** math.ceil(math.log10(max(v for _, v in p99)))
    c.xscale(0, xmax)
    c.yscale(1, hi, log=True)
    phases = sorted({r["rate"] for r in rows})
    starts = [min(r["rel"] for r in rows if r["rate"] == p) for p in phases]
    for i, (p, s0) in enumerate(zip(phases, starts)):
        e = starts[i + 1] if i + 1 < len(starts) else xmax
        if i % 2 == 1:
            c.band(s0, e)
        mine = [r for r in rows if r["rate"] == p]
        refused = sum(r["status"] != 200 for r in mine) / len(mine)
        mid = (c.sx(s0) + c.sx(e)) / 2
        said = f"{refused:.1%} refused" if refused < 0.1 else f"{refused:.0%} refused"
        if p in before:
            # Three lines over the plot: the rate, what was refused, and what
            # round 3 refused at the same rate per node.
            c.text(mid, c.plot[1] - 34, f"{p:g}/s per node", colour=t["ink2"], anchor="middle", weight=600)
            c.text(mid, c.plot[1] - 20, said, anchor="middle", colour=t["ink"])
            c.text(mid, c.plot[1] - 6, f"was {sum(before[p]) / len(before[p]):.0%}", anchor="middle")
        else:
            c.text(mid, c.plot[1] - 6, f"{p * nodes:g} req/s", colour=t["ink2"], anchor="middle", weight=600)
            c.text(mid, c.plot[1] + 14, said, anchor="middle")
    c.ygrid([10 ** k for k in range(0, int(math.log10(hi)) + 1)], ms)
    c.xticks(range(0, int(xmax) + 1, 120), lambda s: f"{s // 60} min")
    c.line(p99, t["series"][1], width=2, smooth=False, dot_end=False)
    c.line(p50, t["series"][0], smooth=False, dot_end=False)
    return c


def heaps(src):
    """Per heap size: kept in memory as a sandbox and as a plain process, and restore times."""
    # Both counts come from heaps/, taken on the same six nodes on the same
    # day, every size measured the same way: in a gVisor sandbox, the hot
    # wall (what the nodes started before holding creates back at their
    # floor); as a plain process, baselines.py started until MemAvailable
    # fell under the same floor. Earlier versions took 512 MiB and 1 GiB
    # from the heavy runs, which started a fixed 2 and 1 per node, and had
    # no plain-process count for them at all.
    sandboxed, bare = {}, {}
    for h in (16, 64, 256, 512, 1024):
        ups = [json.load(open(f))["settled"]["up"] / 6 for f in sorted(src.glob(f"heaps/wall_app{h}-*.json"))]
        if ups:
            sandboxed[h] = sum(ups) / len(ups)
        n = [r["instances"] for f in sorted(src.glob("heaps/*-process-*.jsonl")) for r in jsonl(f)
             if r.get("kind") == "density" and r["app"] == f"app{h}"]
        if n:
            bare[h] = quantile(n, 0.5)
    heavy = runs(src, "heavy")
    restores = {}
    for r in runs(src, "sizes") + heavy:
        if r["kind"] == "restore" and r["status"] == 200:
            restores.setdefault(int(r["template"][3:]), []).append(r["latency_s"] * 1000)
    sizes = [h for h in (16, 64, 256, 512, 1024) if h in sandboxed and h in restores]
    restores = {h: quantile(v, 0.5) for h, v in restores.items()}
    return sizes, sandboxed, bare, restores


def bars(c, sizes, groups, fmt, under=None):
    """Rounded bars per heap size; `groups` is [(values by heap, colour)].
    `under`, by heap, is a second line of text beneath each size."""
    x0, y0, x1, y1 = c.plot
    t = c.t
    c.out.append(f'<line x1="{x0}" x2="{x1}" y1="{y1}" y2="{y1}" stroke="{t["axis"]}"/>')
    slot = (x1 - x0) / len(sizes)
    bw = min(18, slot / (len(groups) + 1.5))
    vmax = max(v for vals, _ in groups for h, v in vals.items() if h in sizes) * 1.18
    for i, h in enumerate(sizes):
        cx = x0 + slot * (i + 0.5)
        present = [(vals[h], colour) for vals, colour in groups if h in vals]
        for j, (v, colour) in enumerate(present):
            bx = cx + (j - (len(present) - 1) / 2) * (bw + 3)
            bh = (y1 - y0 - 18) * v / vmax
            yt, r = y1 - bh, min(4, bh)
            c.out.append(f'<path d="M{bx - bw / 2:.1f},{y1} V{yt + r:.1f} Q{bx - bw / 2:.1f},{yt:.1f} {bx - bw / 2 + r:.1f},{yt:.1f} H{bx + bw / 2 - r:.1f} Q{bx + bw / 2:.1f},{yt:.1f} {bx + bw / 2:.1f},{yt + r:.1f} V{y1} Z" fill="{colour}"/>')
            c.text(bx, yt - 6, fmt(v), size=10, colour=t["ink"], anchor="middle", weight=600)
        c.text(cx, y1 + 20, f"{h} MiB" if h < 1024 else "1 GiB", anchor="middle")
        if under and h in under:
            c.text(cx, y1 + 37, under[h], anchor="middle", colour=t["ink2"])


def byheap(src, theme, w, h):
    """Kept in memory per node, as a sandbox and as a plain process, by heap;
    the median wake from disk as a row of text under each size, not a second
    axis."""
    sizes, sandboxed, bare, restores = heaps(src)
    if not sizes:
        return None
    t = THEMES[theme]
    c = Chart(theme, "By heap size",
              "Kept in memory per node; under each, its wake from disk.",
              legend=[(t["series"][1], "plain process", "dot"), (t["series"][0], "gVisor sandbox", "dot")],
              w=w, h=h, compact=True, left=20, right=20, bottom=54)
    bars(c, sizes, [(bare, t["series"][1]), (sandboxed, t["series"][0])], lambda v: f"{v:.0f}",
         under={k: ms(round(v)) if v < 1000 else f"{v / 1000:.1f} s" for k, v in restores.items()})
    return c


def figure(src, theme):
    """The README's lead image: four panels two by two. Capacity on the left
    (the count held, by heap size), latency on the right (by idle time,
    under traffic)."""
    gap, rows = 16, (300, 300)
    pw = (W - gap) // 2
    head = held(src, theme, pw, rows[0], compact=True)
    panels = [head] + [draw(src, theme, pw, rows[(i + 1) // 2]) for i, draw in enumerate((sleep, byheap, mixed))]
    if not all(panels):
        return None
    place = [(c, (i % 2) * (pw + gap), (rows[0] + gap) * (i // 2)) for i, c in enumerate(panels)]
    out = [f'<g transform="translate({x},{y})">\n' + "\n".join(c.out) + "\n</g>" for c, x, y in place]
    return svg(W, sum(rows) + gap, "Sandboxes held across six machines and kept in memory by heap size, beside latency by idle time and under traffic", "\n".join(out))


def compare(src, theme):
    """How many Python HTTP servers one node holds, by what runs them.

    Every runtime but Sandstorm keeps each one in memory, started one at a
    time until the machine is at the 512 MiB floor Sandstorm holds back at
    (baselines.py, every node, twice). Sandstorm is shown twice: with every
    sandbox kept awake, and with idle ones tiered -- the count the fill run
    reached with every sampled one still answering. Log scale: the last bar
    is two orders past the rest.
    """
    rows = [json.loads(l) for f in sorted((src / builds(src)["baselines"]).glob("*-baselines-*.jsonl")) for l in open(f)]
    made = sorted((src / builds(src)["fills"]).glob("fill-*.jsonl*"))
    made = [f.with_name(f.name.removesuffix(".gz")) for f in made if "-samples" not in f.name]
    awake, fills = walls(src, "web"), jsonl(made[0]) if made else []
    if not rows or not awake or not fills:
        return None
    nodes = 6
    ours = {"web_awake_per_node": awake, "web_held_per_node": sum(r.get("kind") == "created" for r in fills) / nodes}
    per = {}
    for r in rows:
        if r.get("kind") == "density" and r["app"] == "web":
            per.setdefault(r["runtime"], []).append(r["instances"])
    names = [("process", "Plain process, no isolation"), ("bwrap", "bubblewrap"), ("podman", "Podman (crun)"),
             ("runc", "Docker (runc)"), ("runsc", "Docker + gVisor")]
    t = THEMES[theme]
    bars = [(label, quantile(per[k], 0.5), t["muted"], False) for k, label in names if k in per]
    bars += [("Sandstorm, every sandbox awake", ours["web_awake_per_node"], t["series"][0], True),
             ("Sandstorm, idle ones tiered, all callable", ours["web_held_per_node"], t["series"][1], True)]
    h = 96 + 34 * len(bars) + 40
    c = Chart(theme, "Python HTTP servers one 2-core, 2 GB node holds",
              "Kept in memory until 512 MiB is left; median of 12 runs on 6 nodes. Log scale.",
              h=h, left=270, right=60, title_left=24)
    c.xscale(10, 10000, log=True)
    x0, y0, x1, y1 = c.plot
    for v in (10, 100, 1000, 10000):
        X = c.sx(v)
        c.out.append(f'<line x1="{X:.1f}" x2="{X:.1f}" y1="{y0}" y2="{y1}" stroke="{t["grid"] if v > 10 else t["axis"]}"/>')
        c.text(X, y1 + 18, f"{v:,}", anchor="middle")
    for i, (label, v, colour, bold) in enumerate(bars):
        y = y0 + 8 + i * 34
        c.text(x0 - 12, y + 15, label, colour=t["ink"] if bold else t["ink2"], anchor="end", weight=600 if bold else 400)
        w = c.sx(v) - x0
        c.out.append(f'<rect x="{x0}" y="{y + 3}" width="{w:.1f}" height="18" rx="4" fill="{colour}"/>')
        c.text(c.sx(v) + 8, y + 16, f"{v:,.0f}", colour=t["ink"], weight=600)
    return c.svg("Python HTTP servers one node holds, by runtime")


CHARTS = {"overview": figure, "herd": herd, "compare": compare}


def main():
    src, dst = Path(sys.argv[1]), Path(sys.argv[2])
    dst.mkdir(parents=True, exist_ok=True)
    wrote = []
    for name, draw in CHARTS.items():
        for theme in THEMES:
            svg = draw(src, theme)
            if svg is None:
                break
            (dst / f"fleet-{name}-{theme}.svg").write_text(svg)
        else:
            wrote.append(name)
    print(f"wrote {', '.join(wrote) or 'nothing'} to {dst}")


if __name__ == "__main__":
    main()
