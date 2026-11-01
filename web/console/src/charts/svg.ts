// ---- SVG helpers -----------------------------------------------------------
// The charts are drawn by hand into an <svg> a component owns: every draw
// clears it and builds it again from the data.
import type { Point } from "../types";

const NS = "http://www.w3.org/2000/svg";
export function el(tag: string, attrs: Record<string, string | number> = {}, parent?: Element): SVGElement {
  const e = document.createElementNS(NS, tag) as SVGElement;
  for (const k in attrs) e.setAttribute(k, String(attrs[k]));
  if (parent) parent.appendChild(e);
  return e;
}
export function pathOf(pts: Point[], sx: (t: number) => number, sy: (v: number) => number): string {
  return pts.map(([t, v], i) => (i ? "L" : "M") + sx(t).toFixed(1) + "," + sy(v).toFixed(1)).join("");
}
export function niceMax(v: number): number {
  if (!(v > 0)) return 1;
  const p = Math.pow(10, Math.floor(Math.log10(v))), n = v / p;
  return (n <= 1 ? 1 : n <= 2 ? 2 : n <= 2.5 ? 2.5 : n <= 5 ? 5 : 10) * p;
}
export function drawIn(p: SVGPathElement, dur = 1100): void {
  const len = p.getTotalLength ? p.getTotalLength() : 0;
  if (!len) return;
  p.style.strokeDasharray = String(len); p.style.strokeDashoffset = String(len);
  p.getBoundingClientRect();
  p.style.transition = `stroke-dashoffset ${dur}ms cubic-bezier(.2,.8,.2,1)`;
  p.style.strokeDashoffset = "0";
}
