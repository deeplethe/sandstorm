// The nodes' free memory over the range: a strip each, on one scale and one
// time axis, so a column read down is one moment across the fleet.
import type { ConsoleNode, SeriesView } from "../types";
import { clock, fmt, gib } from "../format";
import { css } from "../theme";
import { line } from "../series";
import { hideTip, showTip } from "./tip";

const hex = (s: string): number[] => { s = s.replace("#", ""); return [0, 2, 4].map((i) => parseInt(s.slice(i, i + 2), 16)); };
const mix = (a: number[], b: number[], k: number): number[] => a.map((v, i) => Math.round(v + (b[i] - v) * Math.max(0, Math.min(1, k))));

// One scale for every strip: the most free memory any node had in the range.
function heatScale(all: ConsoleNode[], floor: number): number {
  let vmax = Math.max(1, floor * 2);
  for (const n of all) for (const [, v] of line("node.free_mib", n.id)) vmax = Math.max(vmax, v);
  return vmax;
}
function heatColor(v: number, vmax: number, floor: number): string {
  if (v < floor) return css("--bad");
  const c0 = hex(css("--heat-0")), c1 = hex(css("--heat-1")), c2 = hex(css("--heat-2"));
  const used = 1 - v / vmax;
  const col = used < 0.5 ? mix(c0, c1, used * 2) : mix(c1, c2, (used - 0.5) * 2);
  return `rgb(${col.join(",")})`;
}

export function strips(rows: HTMLElement, legend: HTMLElement, all: ConsoleNode[], S: SeriesView, floor: number): void {
  const vmax = heatScale(all, floor), t0 = S.since, t1 = S.now, dpr = devicePixelRatio || 1;
  for (const c of rows.querySelectorAll<HTMLCanvasElement>("canvas.strip")) {
    const id = c.dataset.id || "", w = c.clientWidth, h = c.clientHeight;
    c.width = w * dpr; c.height = h * dpr;
    const ctx = c.getContext("2d"); if (!ctx) continue;
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.fillStyle = css("--grid"); ctx.fillRect(0, 0, w, h);
    const pts = line("node.free_mib", id), ups = new Map(line("node.up", id));
    const sx = (t: number) => (t - t0) / (t1 - t0) * w, cell = Math.max(1, w / Math.max(1, (t1 - t0) / S.step));
    for (const [t, v] of pts) {
      ctx.fillStyle = ups.get(t) === 0 ? css("--grid") : heatColor(v, vmax, floor);
      ctx.fillRect(sx(t), 0, cell + 0.6, h);
    }
    c.onmousemove = (e) => {
      if (!pts.length) return hideTip();
      const r = c.getBoundingClientRect(), t = t0 + (e.clientX - r.left) / r.width * (t1 - t0);
      let best = pts[0]; for (const p of pts) if (Math.abs(p[0] - t) < Math.abs(best[0] - t)) best = p;
      const silent = ups.get(best[0]) === 0;
      showTip(e, `${id} · ${clock(best[0])}`, [[silent ? "silent" : "free memory", silent ? css("--grid") : heatColor(best[1], vmax, floor), silent ? "–" : fmt(best[1]) + " MiB"]]);
    };
    c.onmouseleave = hideTip;
  }
  // The legend: the same colours, in MiB, with the floor marked.
  const F = Math.min(100, floor / vmax * 100);
  const stops = floor > 0
    ? [`${css("--bad")} 0%`, `${css("--bad")} ${F}%`, `${heatColor(floor, vmax, floor)} ${F}%`, `${heatColor((floor + vmax) / 2, vmax, floor)} ${(F + 100) / 2}%`, `${heatColor(vmax, vmax, floor)} 100%`]
    : [`${heatColor(0, vmax, floor)} 0%`, `${heatColor(vmax / 2, vmax, floor)} 50%`, `${heatColor(vmax, vmax, floor)} 100%`];
  const ticks: [number, string][] = floor > 0 ? [[0, "0"], [F, `${gib(floor)} floor`], [100, gib(vmax)]] : [[0, "0"], [50, gib(vmax / 2)], [100, gib(vmax)]];
  if (floor > 0 && F < 70) ticks.splice(2, 0, [(F + 100) / 2, gib((floor + vmax) / 2)]);
  legend.innerHTML = `<span>Free memory</span><span class="scale"><span class="ramp" style="background:linear-gradient(90deg, ${stops.join(", ")})">${floor > 0 ? `<u style="left:${F}%"></u>` : ""}</span>
    <span class="ticks">${ticks.map(([x, l]) => `<span style="left:${x}%">${l}</span>`).join("")}</span></span>
    ${floor > 0 ? `<span class="key"><i style="background:var(--bad)"></i>under the floor: the master sends it nothing new</span>` : ""}
    <span class="key"><i style="background:var(--grid)"></i>silent</span>`;
}
