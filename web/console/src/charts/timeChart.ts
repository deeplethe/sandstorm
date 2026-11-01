import type { Point, SeriesView } from "../types";
import { clock, compact, fmt } from "../format";
import { css } from "../theme";
import { drawIn, el, niceMax, pathOf } from "./svg";
import { hideTip, showTip } from "./tip";

export interface ChartSeries { label: string; color: string; points: Point[]; dash?: string; width?: number }
export interface ChartOpts { kind?: "line" | "stack"; log?: boolean; unit?: string; first?: boolean }

// A time chart: one x axis of time, one y axis, any number of series.
// `kind` is "line" or "stack". Hover shows a crosshair and every value.
export function timeChart(svg: SVGSVGElement, S: SeriesView, series: ChartSeries[], { kind = "line", log = false, unit = "", first = false }: ChartOpts = {}): void {
  svg.innerHTML = "";
  // The box it was given: fixed by its height attribute on a scrolling
  // page, shared out by the grid on the one-screen overview.
  const w = svg.clientWidth || 600, h = svg.clientHeight || Number(svg.getAttribute("height")) || 240;
  svg.setAttribute("viewBox", `0 0 ${w} ${h}`);
  const L = 44, R = 10, T = 8, B = 22;
  const all = series.flatMap((s) => s.points);
  if (!all.length) { el("text", { x: w / 2, y: h / 2, "text-anchor": "middle" }, svg).textContent = "Collecting… the first points arrive within a few seconds"; return; }
  const t0 = S.since, t1 = S.now;
  const sx = (t: number) => L + (t - t0) / (t1 - t0) * (w - L - R);
  type Band = [number, number, number];
  let stacked: (ChartSeries & { band: Band[] })[] = [];
  if (kind === "stack") {
    const times = [...new Set(all.map((p) => p[0]))].sort((a, b) => a - b);
    const base = new Map<number, number>(times.map((t) => [t, 0]));
    stacked = series.map((s) => {
      const m = new Map(s.points);
      const pts = times.map((t): Band => { const lo = base.get(t) || 0, hi = lo + (m.get(t) || 0); base.set(t, hi); return [t, lo, hi]; });
      return { ...s, band: pts };
    });
  }
  const vmaxRaw = kind === "stack" ? Math.max(...stacked[stacked.length - 1].band.map((p) => p[2])) : Math.max(...all.map((p) => p[1]));
  let sy: (v: number) => number, ticks: number[];
  if (log) {
    const lo = 0.1, hi = Math.pow(10, Math.ceil(Math.log10(Math.max(vmaxRaw, 1))));
    sy = (v) => T + (1 - (Math.log10(Math.max(v, lo)) - Math.log10(lo)) / (Math.log10(hi) - Math.log10(lo))) * (h - T - B);
    ticks = []; for (let v = lo; v <= hi * 1.001; v *= 10) ticks.push(v);
  } else {
    const vmax = niceMax(vmaxRaw * 1.08);
    sy = (v) => T + (1 - v / vmax) * (h - T - B);
    ticks = [0, vmax / 4, vmax / 2, vmax * 3 / 4, vmax];
  }
  for (const [i, v] of ticks.entries()) {
    const y = sy(v);
    el("line", { x1: L, x2: w - R, y1: y, y2: y, class: i === 0 && !log ? "baseline" : "gridline" }, svg);
    el("text", { x: L - 8, y: y + 4, "text-anchor": "end" }, svg).textContent = log ? (v < 1 ? String(v) : compact(v)) : compact(v);
  }
  const nT = Math.max(2, Math.floor((w - L - R) / 110));
  for (let i = 0; i <= nT; i++) {
    const t = t0 + (t1 - t0) * i / nT;
    el("text", { x: sx(t), y: h - 6, "text-anchor": i === 0 ? "start" : i === nT ? "end" : "middle" }, svg).textContent = clock(t);
  }
  const plot = el("g", {}, svg);
  if (kind === "stack") {
    for (const s of stacked) {
      const top = s.band.map(([t, , hi]): Point => [t, hi]), bot = s.band.map(([t, lo]): Point => [t, lo]).reverse();
      const d = pathOf(top, sx, sy) + pathOf(bot, sx, sy).replace("M", "L") + "Z";
      const a = el("path", { d, fill: s.color, "fill-opacity": .85, stroke: css("--card"), "stroke-width": 1.2 }, plot);
      if (first) { a.style.transformOrigin = `0 ${h - B}px`; a.style.transform = "scaleY(0)"; a.getBoundingClientRect(); a.style.transition = "transform 1s cubic-bezier(.2,.8,.2,1)"; a.style.transform = "scaleY(1)"; }
    }
  } else {
    for (const s of series) {
      if (s.points.length < 1) continue;
      const p = el("path", { d: pathOf(s.points, sx, sy), fill: "none", stroke: s.color, "stroke-width": s.width || 2, "stroke-linejoin": "round", "stroke-linecap": "round", "stroke-dasharray": s.dash || "" }, plot) as SVGPathElement;
      if (first && !s.dash) drawIn(p, 1300);
      const lp = s.points[s.points.length - 1];
      el("circle", { cx: sx(lp[0]), cy: sy(lp[1]), r: 3.5, fill: s.color, stroke: css("--card"), "stroke-width": 2 }, plot);
    }
  }
  // hover
  const cross = el("line", { class: "cross", y1: T, y2: h - B, opacity: 0 }, svg);
  const hit = el("rect", { x: L, y: T, width: w - L - R, height: h - T - B, fill: "transparent" }, svg);
  const times = [...new Set(all.map((p) => p[0]))].sort((a, b) => a - b);
  hit.addEventListener("mousemove", (e) => {
    const r = svg.getBoundingClientRect(), x = (e.clientX - r.left) * (w / r.width);
    const t = t0 + (x - L) / (w - L - R) * (t1 - t0);
    let best = times[0]; for (const tt of times) if (Math.abs(tt - t) < Math.abs(best - t)) best = tt;
    cross.setAttribute("x1", String(sx(best))); cross.setAttribute("x2", String(sx(best))); cross.setAttribute("opacity", "1");
    const rows = series.map((s): [ChartSeries, number | null] => { const p = s.points.find((q) => q[0] === best); return [s, p ? p[1] : null]; });
    showTip(e, clock(best), rows.map(([s, v]) => [s.label, s.color, v == null ? "–" : fmt(v, v < 10 ? 2 : 0) + unit]));
  });
  hit.addEventListener("mouseleave", () => { cross.setAttribute("opacity", "0"); hideTip(); });
}
