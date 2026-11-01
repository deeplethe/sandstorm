import type { Point } from "../types";
import { clock } from "../format";
import { css } from "../theme";
import { drawIn, el, pathOf } from "./svg";
import { hideTip, showTip } from "./tip";

export interface SparkTip { label: string; f: (v: number) => string }

// The hover of each sparkline, replaced with every draw; the card listens
// once and calls whichever is current.
const hovers = new WeakMap<SVGSVGElement, { hover: (e: MouseEvent) => void; leave: () => void }>();
const drawn = new WeakSet<SVGSVGElement>();
const listening = new WeakSet<Element>();

// `tip`, if given, is { label, f }: hovering the card then marks the nearest
// point and says what it was, the way the charts do.
export function spark(svg: SVGSVGElement, pts: Point[], color: string, fill = true, tip: SparkTip | null = null): void {
  svg.innerHTML = "";
  const w = svg.clientWidth || 200, h = svg.clientHeight || 46;
  svg.setAttribute("viewBox", `0 0 ${w} ${h}`);
  if (pts.length < 2) return;
  const t0 = pts[0][0], t1 = pts[pts.length - 1][0];
  const vmax = Math.max(...pts.map((p) => p[1])) || 1, vmin = Math.min(0, ...pts.map((p) => p[1]));
  const sx = (t: number) => (t - t0) / Math.max(1, t1 - t0) * w, sy = (v: number) => h - 4 - (v - vmin) / (vmax - vmin || 1) * (h - 10);
  const d = pathOf(pts, sx, sy);
  const id = "g" + Math.random().toString(36).slice(2);
  const defs = el("defs", {}, svg), g = el("linearGradient", { id, x1: 0, y1: 0, x2: 0, y2: 1 }, defs);
  el("stop", { offset: 0, "stop-color": color, "stop-opacity": .28 }, g);
  el("stop", { offset: 1, "stop-color": color, "stop-opacity": 0 }, g);
  if (fill) el("path", { d: d + `L${w},${h}L0,${h}Z`, fill: `url(#${id})` }, svg);
  const p = el("path", { d, fill: "none", stroke: color, "stroke-width": 1.8, "stroke-linejoin": "round", "stroke-linecap": "round" }, svg) as SVGPathElement;
  // Drawn in once, the first time there is a line to draw. Every refresh
  // rebuilds the path, and drawing each one in again replayed the
  // left-to-right sweep on every card every five seconds.
  if (!drawn.has(svg)) { drawIn(p); drawn.add(svg); }
  if (!tip) return;
  const guide = el("line", { y1: 0, y2: h, stroke: css("--axis"), "stroke-width": 1, visibility: "hidden" }, svg);
  const dot = el("circle", { r: 3, fill: color, stroke: css("--page"), "stroke-width": 1.5, visibility: "hidden" }, svg);
  hovers.set(svg, {
    hover: (e) => {
      const r = svg.getBoundingClientRect(), t = t0 + Math.max(0, Math.min(1, (e.clientX - r.left) / r.width)) * (t1 - t0);
      let best = pts[0]; for (const q of pts) if (Math.abs(q[0] - t) < Math.abs(best[0] - t)) best = q;
      const x = sx(best[0]).toFixed(1), y = sy(best[1]).toFixed(1);
      guide.setAttribute("x1", x); guide.setAttribute("x2", x); guide.setAttribute("visibility", "visible");
      dot.setAttribute("cx", x); dot.setAttribute("cy", y); dot.setAttribute("visibility", "visible");
      showTip(e, clock(best[0]), [[tip.label, color, tip.f(best[1])]]);
    },
    leave: () => { guide.setAttribute("visibility", "hidden"); dot.setAttribute("visibility", "hidden"); hideTip(); },
  });
  const card = svg.parentElement;
  if (card && !listening.has(card)) {
    listening.add(card);
    card.addEventListener("mousemove", (e) => hovers.get(svg)?.hover(e));
    card.addEventListener("mouseleave", () => hovers.get(svg)?.leave());
  }
}
