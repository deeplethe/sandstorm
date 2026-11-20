// What the console knows of the fleet, and the refresh that keeps it.
//
// One small store: the polled `NOW` (`/v1/console/now`) and `S`
// (`/v1/console/series`), the tab, the overview's host group, the live
// status, the gate. Components read it directly and re-render when it says
// it changed; each table's own filters and pages stay in its component.
import { useSyncExternalStore } from "react";
import type { ConsoleNow, SeriesView } from "./types";
import { api, ApiError, auth, setToken } from "./api";
import { local, view } from "./storage";
import { validPool } from "./groups";
import { loading } from "./loading";
import { hms } from "./format";

// In the hash, so a reload or a shared link lands on the same one.
export const TABS = ["overview", "nodes", "pools", "sandboxes", "snapshots", "activity"] as const;
export type Tab = (typeof TABS)[number];
const asTab = (h: string): Tab => ((TABS as readonly string[]).includes(h) ? (h as Tab) : "overview");

export const st = {
  NOW: null as ConsoleNow | null,
  S: null as SeriesView | null,
  tab: asTab(location.hash.slice(1)),
  // ---- the overview's host group ---------------------------------------------
  // With a host group picked, what the overview counts is that group's: its
  // nodes now, and over time the per-node lines of whichever nodes were in it
  // at each moment (`pool_history`), added up into the fleet's own line names
  // so every chart draws them unchanged. Rates and latencies are only kept for
  // the whole fleet, and say so.
  pool: local.get("sandstorm-console-pool") || "",
  lastOk: 0,
  liveErr: null as unknown,
  live: { dot: "dot", text: "connecting" },
  gate: { open: false, msg: "", seq: 0 },
  // Bumped whenever everything drawn by hand should be drawn again: new data,
  // a theme, a resize, a tab that has just been shown. `first` is the draw
  // that animates in.
  draw: { v: 0, first: false },
  // Bumped by every good refresh, for the tables that load their own page.
  refreshed: 0,
};

let version = 0;
const listeners = new Set<() => void>();
export function emit(): void {
  version++;
  for (const l of listeners) l();
}
const subscribe = (l: () => void) => { listeners.add(l); return () => { listeners.delete(l); }; };
/** Re-renders the caller whenever the store changes. */
export function useStore(): number {
  return useSyncExternalStore(subscribe, () => version);
}

// ---- the gate ------------------------------------------------------------------
export function showGate(msg?: string): void {
  st.gate = { open: true, msg: msg || "", seq: st.gate.seq + 1 };
  emit();
}
auth.refused = () => showGate("That token was refused.");
export function signIn(token: string): void {
  refreshGeneration++;
  setToken(token);
  st.gate = { ...st.gate, open: false };
  emit();
  void refresh(true);
}
export function signOut(): void {
  refreshGeneration++;
  setToken(null);
  showGate();
}

// ---- tabs, range, group --------------------------------------------------------
export function setTab(h: string): void {
  st.tab = asTab(h);
  emit();
}
export function setRange(s: number): void {
  if (s !== view.range) refreshGeneration++;
  view.range = s;
  local.set("sandstorm-console-range", String(s));
  emit();
  void refresh(true);
}
export function setPool(v: string): void {
  st.pool = v;
  if (v) local.set("sandstorm-console-pool", v); else local.remove("sandstorm-console-pool");
  if (st.NOW && st.S) redrawAll(); else emit();
}

export function redrawAll(first = false): void {
  if (!st.S || !st.NOW) return;
  st.draw = { v: st.draw.v + 1, first };
  emit();
}

// The time of the last good refresh, which changes only when there is a new
// one -- not a count of seconds that changed every second. The dot turns
// amber, then red, as that time grows old.
export function setLive(err?: unknown): void {
  if (err !== undefined) st.liveErr = err;
  const age = (Date.now() - st.lastOk) / 1000;
  const dot = "dot" + (st.liveErr && age > 15 ? " dead" : age > 12 ? " stale" : "");
  const text = !st.lastOk ? (st.liveErr ? "cannot reach the master" : "connecting")
    : st.liveErr && age > 15 ? `cannot reach the master · last ${hms(st.lastOk)}` : `updated ${hms(st.lastOk)}`;
  if (dot !== st.live.dot || text !== st.live.text) { st.live = { dot, text }; emit(); }
}

// ---- one refresh ------------------------------------------------------------
// The loading screen: at once on the first load, and only if a new range
// is slow to come (a quick one would just flash). It closes on an answer
// from the refresh that opened it or a later one, never an older one.
let refreshGeneration = 0, seq = 0, settled = 0, openedBy = 0, firstDraw = true;
const RANGE_NAME: Record<number, string> = { 900: "15m", 3600: "1h", 21600: "6h", 86400: "24h" };
export async function refresh(first = false): Promise<void> {
  if (!auth.token) { showGate(); return; }
  let slow = 0;
  const range = view.range, generation = refreshGeneration, id = ++seq;
  // A newer pending poll can take longer than the poll interval. Accept
  // results in completion order until a newer request has already settled.
  const isCurrent = () => generation === refreshGeneration && id > settled;
  const open = (msg?: string) => {
    if (isCurrent() && id >= openedBy) { openedBy = id; loading(true, msg); }
  };
  const close = () => {
    if (id >= openedBy) { openedBy = 0; loading(false); }
  };
  if (first) {
    if (!st.lastOk) open();
    else slow = window.setTimeout(() => open("Loading the last " + (RANGE_NAME[range] || "")), 220);
  }
  try {
    const options = { showGateOnUnauthorized: false };
    const [now, series] = await Promise.all([
      api<ConsoleNow>("/v1/console/now", undefined, options),
      api<SeriesView>(`/v1/console/series?since=${range}` + (range <= 900 ? "&step=5" : ""), undefined, options),
    ]);
    if (!isCurrent()) return;
    const v = validPool(now.nodes, st.pool);
    settled = id;
    st.NOW = now; st.S = series; st.lastOk = Date.now();
    // A group that is gone is forgotten rather than kept, showing nothing.
    if (v !== st.pool) { st.pool = v; local.remove("sandstorm-console-pool"); }
    st.refreshed++;
    redrawAll(first || firstDraw); firstDraw = false;
    setLive(null);
    close();
  } catch (e) {
    if (!isCurrent()) return;
    settled = id;
    if (e instanceof ApiError && e.status === 401) auth.refused();
    else { console.warn(e); setLive(e); }
    if (st.lastOk) close();
    else if (id >= openedBy) {
      openedBy = id;
      loading(true, "Cannot reach the master. Trying again every 5 seconds.", true);
    }
  } finally {
    clearTimeout(slow);
  }
}
