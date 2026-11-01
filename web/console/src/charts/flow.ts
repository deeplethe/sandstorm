// ---- the tier flow ------------------------------------------------------------
// The tiers in rows: idle carries a sandbox one row down at a time --
// awake, frozen, on disk, detached -- and a request wakes it back to awake
// from wherever it is, a return up the right-hand gutter with its own rate.
// Every rate is measured over the last half-minute. Deletes leave from any
// tier, so they are a footnote rather than a line.
import type { Tier, TierCounts } from "../types";
import { dur, fmt, perSec } from "../format";
import { TIERS, TIER_LABEL, css, tierColor } from "../theme";
import { line, recent, smooth } from "../series";
import { el } from "./svg";
import { spark } from "./spark";
import { tweenText } from "./tween";
import { hideTip, showTip, type TipRow } from "./tip";

export type FlowRates = Record<string, number>;

// [rate key, the tier it goes to, verb, what it means]
export const FLOW_MOVES: [string, Tier, string, string][] = [["create", "hot", "new", "Sandboxes start awake when they are created."],
  ["frozen", "frozen", "freeze", "Awake sandboxes left idle are frozen: paused in memory."],
  ["snapshot", "snapshot", "to disk", "Frozen sandboxes left idle longer are checkpointed to disk and leave memory."],
  ["detached", "detached", "detach", "Sandboxes on disk left idle longest are detached from their node."],
  ["wake_frozen", "hot", "wake", "A request to a frozen sandbox resumes it in memory."],
  ["wake_snapshot", "hot", "wake", "A request to a sandbox on disk restores it on its node."],
  ["wake_detached", "hot", "wake", "A request to a detached sandbox places it on a node with room and restores it."]];

// The flow reads the newest rates.
export function flowRates(): FlowRates {
  return {
    create: recent(line("rate.creates", "placed")),
    frozen: recent(line("rate.demotions", "frozen")),
    snapshot: recent(line("rate.demotions", "snapshot")),
    detached: recent(line("rate.demotions", "detached")),
    wake: recent(line("rate.wakes")),
    wake_frozen: recent(line("rate.wakes", "frozen")),
    wake_snapshot: recent(line("rate.wakes", "snapshot")),
    wake_detached: recent(line("rate.wakes", "detached")),
    delete: recent(line("rate.deletes")),
  };
}

export function flowLabel(c: TierCounts, r: FlowRates): string {
  return TIERS.map((t) => `${TIER_LABEL[t]} ${fmt(c[t] || 0)}`).join(", ") +
    `; new ${perSec(r.create || 0)}, freezing ${perSec(r.frozen || 0)}, to disk ${perSec(r.snapshot || 0)}, detaching ${perSec(r.detached || 0)}; woken from frozen ${perSec(r.wake_frozen || 0)}, from disk ${perSec(r.wake_snapshot || 0)}, from detached ${perSec(r.wake_detached || 0)}`;
}

// What the boxes and the rates say on hover, from the last draw.
export function flowHover(e: MouseEvent, c: TierCounts, r: FlowRates, idle: [number, number, number] | null): void {
  const target = e.target as Element, total = TIERS.reduce((a, t) => a + (c[t] || 0), 0);
  const t = target.closest<HTMLElement>(".tnode"), m = target.closest<HTMLElement>(".elab");
  if (t) {
    const id = t.dataset.t as Tier, i = TIERS.indexOf(id);
    const rows: TipRow[] = [["sandboxes", tierColor(id), fmt(c[id] || 0)], ["of the fleet", tierColor(id), total ? fmt(100 * (c[id] || 0) / total, 1) + "%" : "–"]];
    if (id === "hot") rows.push(["created", tierColor("hot"), perSec(r.create || 0)], ["woken by a request", tierColor("hot"), perSec(r.wake || 0)]);
    else {
      if (idle) rows.push(["after idle", css("--muted"), dur(idle[i - 1])]);
      rows.push(["woken from here", tierColor("hot"), perSec(r["wake_" + id] || 0)]);
    }
    return showTip(e, TIER_LABEL[id], rows);
  }
  if (m) {
    const mv = FLOW_MOVES.find((x) => x[0] === m.dataset.k);
    if (mv) {
      const [k, to, verb, what] = mv;
      const from = k.startsWith("wake_") ? ` from ${TIER_LABEL[k.slice(5) as Tier].toLowerCase()}` : "";
      return showTip(e, `${verb}${from} · ${what}`, [["now", tierColor(to), perSec(r[k] || 0)]]);
    }
  }
  hideTip();
}

// The lines, from where the boxes are now: the grid sizes them, this joins them.
export function flowEdges(box: HTMLElement, r: FlowRates): void {
  const svg = box.querySelector<SVGSVGElement>("svg.edges");
  if (!svg || !box.offsetWidth) return;
  svg.innerHTML = "";
  if (getComputedStyle(svg).display === "none") return;
  const o = box.getBoundingClientRect();
  const R = (t: Tier) => {
    const b = box.querySelector(`.tnode[data-t="${t}"]`)!.getBoundingClientRect();
    return { l: b.left - o.left, r: b.right - o.left, t: b.top - o.top, b: b.bottom - o.top, cx: (b.left + b.right) / 2 - o.left, cy: (b.top + b.bottom) / 2 - o.top };
  };
  const boxes = TIERS.map(R), A = boxes[0], g = 6;
  // Thickness on one scale for every move, so a busy line reads as busy
  // next to a quiet one; square root, or one burst flattens the rest.
  const top = Math.max(...FLOW_MOVES.map(([k]) => r[k] || 0)) || 1;
  // pts is a polyline; its corners are rounded and its last leg gets the head
  const line = (pts: [number, number][], k: string) => {
    const on = r[k] > 0, color = on ? tierColor(FLOW_MOVES.find((x) => x[0] === k)![1]) : css("--axis");
    const w = on ? 1 + 1.25 * Math.sqrt((r[k] || 0) / top) : 1, rad = 10;
    let d = `M${pts[0][0]},${pts[0][1]}`;
    for (let i = 1; i < pts.length; i++) {
      const [x, y] = pts[i], nx = pts[i + 1];
      if (!nx) { d += `L${x},${y}`; break; }
      const [px, py] = pts[i - 1], a = Math.min(rad, Math.hypot(x - px, y - py) / 2, Math.hypot(nx[0] - x, nx[1] - y) / 2);
      const ux = Math.sign(x - px), uy = Math.sign(y - py), vx = Math.sign(nx[0] - x), vy = Math.sign(nx[1] - y);
      d += `L${x - ux * a},${y - uy * a}Q${x},${y} ${x + vx * a},${y + vy * a}`;
    }
    const [p, q] = pts.slice(-2), L = Math.hypot(q[0] - p[0], q[1] - p[1]) || 1, ux = (q[0] - p[0]) / L, uy = (q[1] - p[1]) / L, h = 4 + w;
    d += `M${q[0] - h * ux - 0.6 * h * uy},${q[1] - h * uy + 0.6 * h * ux}L${q[0]},${q[1]}L${q[0] - h * ux + 0.6 * h * uy},${q[1] - h * uy - 0.6 * h * ux}`;
    const a: Record<string, string | number> = { d, fill: "none", stroke: color, "stroke-width": w, "stroke-linecap": "round", "stroke-linejoin": "round" };
    if (!on) a["stroke-dasharray"] = "3 4";
    el("path", a, svg);
  };
  const at = (k: string, x: number, y: number, how = "") => {
    const l = box.querySelector<HTMLElement>(`#f-l-${k}`); if (!l) return;
    l.style.left = x + "px"; l.style.top = y + "px"; l.className = `elab ${how}${r[k] > 0 ? "" : " none"}`;
  };
  // in, and idle one row down: connectors down the middle of the left-hand
  // gutter, "in" from above and each move after it leaving its row below the
  // middle and entering the next above it. Every rate sits on its line, in
  // one column.
  const lx = A.l / 2, dy = 10;
  line([[lx, 0], [lx, A.cy - dy], [A.l - g, A.cy - dy]], "create"); at("create", lx, (A.cy - dy) / 2);
  for (let i = 1; i < TIERS.length; i++) {
    const p = boxes[i - 1], q = boxes[i], k = TIERS[i];
    line([[p.l - g, p.cy + dy], [lx, p.cy + dy], [lx, q.cy - dy], [q.l - g, q.cy - dy]], k); at(k, lx, (p.cy + q.cy) / 2);
  }
  // woken, up the right-hand gutter to awake: the nearest tier innermost and
  // entering awake lowest, so no two returns ever cross. Each rate is on its
  // own line just above where it leaves its row, where only lines from further
  // down run beside it, far enough out to clear the label.
  const gap = 22, rx = A.r + (box.clientWidth - A.r - gap * 2) / 2;
  for (let i = 1; i < TIERS.length; i++) {
    const s = boxes[i], k = "wake_" + TIERS[i], gx = rx + gap * (i - 1), ey = A.cy + 12 * (2 - i);
    line([[s.r + g, s.cy], [gx, s.cy], [gx, ey], [A.r + g, ey]], k); at(k, gx, (boxes[i - 1].cy + s.cy) / 2);
  }
}

export function drawFlow(box: HTMLElement, c: TierCounts, r: FlowRates, idle: [number, number, number] | null): void {
  const q = <T extends Element>(id: string) => box.querySelector<T>("#" + id)!;
  const total = TIERS.reduce((a, t) => a + (c[t] || 0), 0);
  const hint: Record<Tier, string> = { hot: "running", frozen: idle ? `idle ${dur(idle[0])}+` : "", snapshot: idle ? `idle ${dur(idle[1])}+` : "", detached: idle ? `idle ${dur(idle[2])}+` : "" };
  for (const t of TIERS) {
    const share = total ? (c[t] || 0) / total : 0;
    tweenText(q<HTMLElement>("f-cnt-" + t), c[t] || 0, 0, "", true);
    q<HTMLElement>("f-hint-" + t).textContent = hint[t];
    q<HTMLElement>("f-pct-" + t).textContent = total ? `${fmt(100 * share, share > 0 && share < 0.01 ? 1 : 0)}%` : "";
    q<HTMLElement>("f-bar-" + t).style.width = (share > 0 ? Math.max(1.5, 100 * share) : 0) + "%";
    // smoothed to the box's width: a long range's 5 s jitter is noise here
    const pts = line("fleet.sandboxes", t);
    spark(q<SVGSVGElement>("f-sp-" + t), smooth(pts, Math.max(1, Math.round(pts.length / 40))), tierColor(t), true);
  }
  for (const [k] of FLOW_MOVES) {
    q<HTMLElement>("f-r-" + k).textContent = r[k] > 0 ? perSec(r[k]) : "0";
    q<HTMLElement>("f-l-" + k).classList.toggle("none", !(r[k] > 0));
  }
  flowEdges(box, r);
}
