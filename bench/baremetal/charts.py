#!/usr/bin/env python3
"""Draw the README's figure from a results directory.

    python3 -m pip install seaborn matplotlib
    python3 bench/baremetal/charts.py bench/baremetal/results/2026-10-30 docs/assets/charts

Written as `compare-light.svg` and `compare-dark.svg`, its text as paths so it
looks the same on any machine, and skipped, not faked, if its data is missing.
Each runtime's figure is its last row in the directory, so a re-run supersedes
the one before it. Its notes are the README's caption, not part of the image.
"""

import json
import sys
from pathlib import Path

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
import seaborn as sns  # noqa: E402
from matplotlib.ticker import FuncFormatter  # noqa: E402

# Each runtime's name in the figure, and whether it is Sandstorm.
ROWS = [
    ("sandstorm-fc-warm", "Sandstorm FC warm", True),
    ("sandstorm-fc", "Sandstorm FC cold", True),
    ("sandstorm-gvisor-warm", "Sandstorm gVisor warm", True),
    ("sandstorm-gvisor", "Sandstorm gVisor cold", True),
    ("cubesandbox", "Cube warm", False),
    ("cubesandbox-cold", "Cube cold", False),
    ("microsandbox", "microsandbox", False),
    ("kata", "Kata", False),
    ("runsc", "Docker + gVisor", False),
    ("runc", "Docker", False),
    ("podman", "Podman", False),
    ("bwrap", "bubblewrap", False),
    ("process", "Process", False),
]
THEMES = {
    "light": {"bg": "#ffffff", "ink": "#24292f", "muted": "#6e7781", "grid": "#e6e6e6", "rest": "#b8bcc2", "ours": "#4c72b0", "first": "#d39b12"},
    "dark": {"bg": "#0d1117", "ink": "#d7dde3", "muted": "#8b949e", "grid": "#21262d", "rest": "#3d444d", "ours": "#3f7fd0", "first": "#d9a93a"},
}


def rows(src):
    for f in sorted(src.glob("*.jsonl")):
        for line in open(f):
            if line.strip():
                yield json.loads(line)


def summaries(src):
    """runtime -> app -> (start p50 ms, MiB per instance), last row wins."""
    out = {}
    for r in rows(src):
        if r.get("kind") != "summary":
            continue
        start = (r.get("start") or {}).get("p50_ms")
        mem = (r.get("memory") or {}).get("per_instance_mib")
        prev = out.setdefault(r["runtime"], {}).get(r["app"], (None, None))
        out[r["runtime"]][r["app"]] = (start or prev[0], mem or prev[1])
    return out


def fleet_summaries(src, sub="fleet"):
    """The same, the median across nodes: one file per node in `fleet/`."""
    per = {}
    for f in sorted((src / sub).glob("*.jsonl")):
        for rt, apps in summaries_of(f).items():
            for app, (st, mem) in apps.items():
                per.setdefault(rt, {}).setdefault(app, ([], []))
                if st:
                    per[rt][app][0].append(st)
                if mem:
                    per[rt][app][1].append(mem)
    med = lambda xs: sorted(xs)[len(xs) // 2] if len(xs) % 2 else sum(sorted(xs)[len(xs) // 2 - 1:len(xs) // 2 + 1]) / 2 if xs else None
    return {rt: {app: (med(a), med(b)) for app, (a, b) in apps.items()} for rt, apps in per.items()}, \
        len(list((src / sub).glob("*.jsonl")))


def summaries_of(f):
    out = {}
    for line in open(f):
        if not line.strip():
            continue
        r = json.loads(line)
        if r.get("kind") == "summary":
            out.setdefault(r["runtime"], {})[r["app"]] = (
                (r.get("start") or {}).get("p50_ms"), (r.get("memory") or {}).get("per_instance_mib"))
    return out


def clones(src):
    """runtime -> (snapshot, [clone 1, 10, 20]) in ms, until every child
    answers. For Sandstorm a branch then one fork call, so their sum."""
    out = {}
    for r in rows(src):
        if r.get("kind") != "fork":
            continue
        s = r.get("snapshot_ms", 0)
        if "clone1_ms" in r:
            out[r["runtime"]] = (s, [r["clone1_ms"], r["clone10_wall_ms"], r["clone20_wall_ms"]])
        else:
            out[r["runtime"]] = (s, [s + r["fork1_ms"], s + r["fork10_wall_ms"], s + r["fork20_wall_ms"]])
    return out


MS = lambda v: f"{v / 1000:.1f} s" if v >= 1000 else f"{v:.0f} ms"
MIB = lambda v: f"{v:.0f} MiB"
TICK = lambda v, _: f"{v / 1000:g}k" if v >= 1000 else f"{v:g}"


def panel(ax, t, title, rows, fmt, lim):
    """One ranked panel: best first, log scale, each value at its bar's end."""
    y = range(len(rows))
    ax.barh(list(y), [v - lim[0] for v, _, _ in rows], left=lim[0], height=0.62, zorder=3,
            color=[t["first"] if i == 0 else t["ours"] if ours else t["rest"] for i, (_, _, ours) in enumerate(rows)],
            edgecolor="none", linewidth=0)
    ax.set_xscale("log")
    ax.set_xlim(*lim)
    ax.set_ylim(len(rows) - 0.4, -0.6)
    ax.set_yticks(list(y), [name for _, name, _ in rows], fontsize=11.5)
    ax.set_title(title, loc="left", fontsize=13, fontweight="bold", color=t["ink"], pad=8)
    ax.tick_params(axis="y", length=0)
    ax.tick_params(axis="x", labelsize=10, colors=t["muted"], length=3)
    ax.grid(axis="y", visible=False)
    ax.grid(axis="x", which="major", color=t["grid"], zorder=0)
    ax.xaxis.set_major_formatter(FuncFormatter(TICK))
    ax.xaxis.set_minor_formatter(FuncFormatter(lambda *_: ""))
    for i, (v, _, _) in enumerate(rows):
        # On the page's colour, so a gridline does not run through the number.
        ax.text(v * 1.14, i, fmt(v), va="center", fontsize=10.5, color=t["ink"], zorder=4,
                bbox=dict(facecolor=t["bg"], edgecolor="none", pad=1.2))
    sns.despine(ax=ax, left=True, bottom=True)


def compare(src, theme, path):
    """Five rankings of the 64 MiB app, each in its own order, since one order
    cannot rank them all; a row is the same height in every panel."""
    kvm = summaries(src)
    cloud, nodes = fleet_summaries(src)
    pvm, _ = fleet_summaries(src, "pvm")
    cl = clones(src)
    if not nodes or len(kvm) < 2 or "sandstorm-fc" not in cl:
        return False
    app = lambda d, k: d.get(k, {}).get("app64", (None, None))
    fork = {"sandstorm-fc-warm": "sandstorm-fc", "sandstorm-gvisor-warm": "sandstorm-gvisor", "cubesandbox": "cubesandbox", "microsandbox": "microsandbox"}
    forked = lambda k, f: f(cl[fork[k]]) if k in fork and fork[k] in cl else None
    t = THEMES[theme]
    sns.set_theme(style="whitegrid", font="Arial", rc={
        "axes.facecolor": t["bg"], "figure.facecolor": t["bg"], "axes.edgecolor": t["grid"], "text.color": t["ink"],
        "xtick.color": t["ink"], "ytick.color": t["ink"], "svg.fonttype": "path"})
    fig = plt.figure(figsize=(15, 5.5))
    gs = fig.add_gridspec(13, 4, left=0.125, right=0.985, top=0.93, bottom=0.07, wspace=1.02, hspace=0)
    panels = [(gs[:13, 0], "Start, KVM machine", lambda k: app(kvm, k)[0], MS, (30, 20000)),
              (gs[:11, 1], "Start, cloud VM", lambda k: app(cloud, k)[0] or app(pvm, k)[0], MS, (30, 20000)),
              (gs[:13, 2], "Memory per instance", lambda k: app(kvm, k)[1], MIB, (5, 2000)),
              (gs[:4, 3], "Clone into 20", lambda k: forked(k, lambda c: c[1][2]), MS, (100, 20000)),
              (gs[6:9, 3], "Snapshot", lambda k: forked(k, lambda c: c[0] or None), MS, (10, 4000))]
    for slot, title, get, fmt, lim in panels:
        rows = sorted(((get(k), name, ours) for k, name, ours in ROWS if get(k)), key=lambda r: r[0])
        panel(fig.add_subplot(slot), t, title, rows, fmt, lim)
    fig.savefig(path, facecolor=t["bg"])
    plt.close(fig)
    return True


def main():
    src, dst = Path(sys.argv[1]), Path(sys.argv[2])
    dst.mkdir(parents=True, exist_ok=True)
    wrote = [compare(src, theme, dst / f"compare-{theme}.svg") for theme in THEMES]
    print(f"wrote compare to {dst}" if all(wrote) else "wrote nothing: data is missing")


if __name__ == "__main__":
    main()
