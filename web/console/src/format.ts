import { view } from "./storage";

export const fmt = (n: number | null | undefined, d = 0): string =>
  n == null || !isFinite(n) ? "–" : Number(n).toLocaleString(undefined, { maximumFractionDigits: d, minimumFractionDigits: d });
export const ago = (s: number): string =>
  s < 60 ? `${Math.round(s)}s ago` : s < 3600 ? `${Math.round(s / 60)}m ago` : `${Math.round(s / 3600)}h ago`;
// Seconds only on the shortest range, where they are what tells points apart.
export const clock = (t: number): string =>
  new Date(t * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: view.range <= 900 ? "2-digit" : undefined });
export const hms = (ms: number): string => new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
export const ease = (t: number): number => 1 - Math.pow(1 - t, 3);
export function dur(s: number): string {
  return s < 120 ? s + " s" : s < 7200 ? Math.round(s / 60) + " min" : Math.round(s / 3600) + " h";
}
export const perSec = (v: number): string =>
  v >= 1 ? fmt(v, 1) + "/s" : v > 0 ? fmt(v * 60, v * 60 < 10 ? 1 : 0) + "/min" : "none";
export const compact = (v: number): string =>
  v >= 1e6 ? (v / 1e6).toFixed(v >= 1e7 ? 0 : 1) + "M" : v >= 1e3 ? (v / 1e3).toFixed(v >= 1e4 ? 0 : 1) + "k" : fmt(v, v < 10 && v % 1 ? 1 : 0);
export const gib = (m: number): string => (m >= 1024 ? fmt(m / 1024, m >= 10240 ? 0 : 1) + "G" : fmt(m) + "M");
export const mib = (b: number): string =>
  b >= 2 ** 30 ? (b / 2 ** 30).toFixed(1) + " GiB" : (b / 2 ** 20).toFixed(b < 10 * 2 ** 20 ? 1 : 0) + " MiB";

// What went wrong, in words: the API answers {"error": "..."}, and some
// paths hand on the body as it came.
export function errorText(e: unknown): string {
  let m = String((e as { message?: unknown })?.message || e);
  try { m = (JSON.parse(m) as { error?: string }).error || m; } catch { /* not JSON */ }
  return m;
}

// For the few things still drawn as markup strings -- the node headroom and
// the heat legend -- rather than as elements.
export const escapeHtml = (s: unknown): string =>
  String(s).replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" })[c] as string);
