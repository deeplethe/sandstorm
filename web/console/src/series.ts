// ---- series ---------------------------------------------------------------
// Reading lines out of the polled history, for the whole fleet or the
// overview's host group.
import type { ConsoleNow, Point, Tier } from "./types";
import { st } from "./state";
import { poolMatch } from "./groups";
import { TIERS } from "./theme";

export function line(metric: string, key = ""): Point[] {
  if (st.pool && metric === "fleet.sandboxes") return poolLine(key === "hot" ? "node.hot" : "node." + key);
  if (st.pool && metric === "fleet.nodes") return key === "down" ? poolLine("node.up", "down", (v) => 1 - v) : poolLine("node.up");
  const S = st.S, l = S && S.lines.find((x) => x.metric === metric && x.key === key);
  return l ? l.points : [];
}

let poolCache: { s: unknown; pool: string; lines: Map<string, Point[]> } = { s: null, pool: "", lines: new Map() };
function inPool(node: string, t: number): boolean {
  return (st.NOW ? st.NOW.pool_history : []).some((p) => p.node === node && poolMatch(st.pool, p.pool) && p.since <= t && (p.until == null || t < p.until));
}
// A fleet line rebuilt from the per-node ones, summed over the nodes in the
// group at each moment. `fk` names `f` in the cache, so the same metric read
// two ways is kept as two lines.
function poolLine(metric: string, fk = "", f: (v: number) => number = (v) => v): Point[] {
  const S = st.S, key = st.pool + "|" + metric + "|" + fk;
  if (poolCache.s !== S || poolCache.pool !== st.pool) poolCache = { s: S, pool: st.pool, lines: new Map() };
  const hit = poolCache.lines.get(key);
  if (hit) return hit;
  const base = S ? S.lines.find((x) => x.metric === "fleet.sandboxes" && x.key === "hot") : null;
  const sum = new Map<number, number>((base ? base.points : []).map(([t]) => [t, 0]));
  for (const l of S ? S.lines : []) {
    if (l.metric !== metric) continue;
    for (const [t, v] of l.points) if (inPool(l.key, t)) sum.set(t, (sum.get(t) || 0) + f(v));
  }
  const out = [...sum.entries()].sort((a, b) => a[0] - b[0]);
  poolCache.lines.set(key, out);
  return out;
}

// What the overview shows now: the whole of `now`, or the picked group's part.
export function scoped(now: ConsoleNow): ConsoleNow {
  if (!st.pool) return now;
  const nodes = now.nodes.filter((n) => poolMatch(st.pool, n.pool));
  const tiers: Partial<Record<Tier, number>> = {};
  for (const n of nodes) for (const t of TIERS) tiers[t] = (tiers[t] || 0) + (n.tiers[t] || 0);
  return { ...now, nodes, tiers };
}

// Rates between two 5 s samples are spiky by nature; a short moving average
// shows the trend without hiding a burst.
export function smooth(pts: Point[], n = 3): Point[] {
  return pts.map(([t], i) => { const a = pts.slice(Math.max(0, i - n + 1), i + 1); return [t, a.reduce((s, p) => s + p[1], 0) / a.length]; });
}
export function last(points: Point[]): number | null { return points.length ? points[points.length - 1][1] : null; }
// The recent average, so the flow moves at the rate of the last half minute
// rather than at whatever one 5 s sample happened to catch.
export function recent(points: Point[], n = 6): number { const a = points.slice(-n); return a.length ? a.reduce((s, p) => s + p[1], 0) / a.length : 0; }
export function sumAt(metric: string, ks: readonly string[]): Point[] {
  const map = new Map<number, number>();
  for (const k of ks) for (const [t, v] of line(metric, k)) map.set(t, (map.get(t) || 0) + v);
  return [...map.entries()].sort((a, b) => a[0] - b[0]);
}

// The admission thresholds the master refuses at, from the master. A floor
// of 0 is none, and a ceiling past any real reading is none either.
export const floorMib = (): number => (st.NOW && st.NOW.floor_mib != null ? st.NOW.floor_mib : 512);
export const maxPsi = (): number => (st.NOW && st.NOW.max_psi != null ? st.NOW.max_psi : 8);
export const hasCeil = (): boolean => maxPsi() < 100;
