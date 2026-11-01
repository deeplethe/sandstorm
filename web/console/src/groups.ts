// ---- host groups ------------------------------------------------------------
// Nodes are grouped by host: the engine, kernel and CPU that decide where a
// snapshot restores (ADR 6). The master names each group by the pool the
// node asked for and, for a host that differs from the rest, its fingerprint
// (`default#3f9a12c4`); nobody picks these, so the console shows a group by
// what its hosts are rather than by that name. New sandboxes go to any group;
// a fork only to the group its snapshot was taken in.
import type { ConsoleNode, HostInfo } from "./types";
import { TIERS } from "./theme";

export const poolMatch = (sel: string, pool: string): boolean => !sel || sel === pool;
export const cpuName = (m: string | undefined): string =>
  (m || "").replace(/\((R|TM)\)/g, "").replace(/ CPU @.*| Processor| \d+-Core/g, "").replace(/\s+/g, " ").trim();
// Which backend a node runs, from its engine's `--version`: gVisor reports
// `runsc version release-…`, Firecracker `Firecracker v…`. The engine is part
// of the fingerprint, so the two never share a group.
export function engineName(e: string | undefined): string {
  const r = /^runsc version (?:release-)?(.*)$/i.exec(e || ""), f = /^firecracker\s+v?(.*)$/i.exec(e || "");
  return r ? `gVisor ${r[1]}` : f ? `Firecracker ${f[1]}` : e || "unknown";
}
export function hostLabel(h: HostInfo | null | undefined): string {
  if (!h) return "";
  return [cpuName(h.cpu_model) || h.cpu_vendor, engineName(h.engine), `kernel ${h.kernel}`].filter(Boolean).join(" · ");
}
export const flagsOf = (h: HostInfo | null): string[] => (h && h.cpu_flags ? h.cpu_flags.split(/\s+/).filter(Boolean) : []);
export const sandboxesOn = (n: ConsoleNode): number => TIERS.reduce((a, t) => a + (n.tiers[t] || 0), 0);

export interface Group {
  pool: string;
  nodes: ConsoleNode[];
  /** Each distinct host, as JSON, and how many nodes report it. */
  hosts: Map<string, number>;
  /** The host most of its nodes report, or null before any has. */
  host: HostInfo | null;
  fp: string;
  up: number;
  sandboxes: number;
  free: number;
  label: string;
}

// The groups, largest first, from the nodes the master sends.
export function poolGroups(all: ConsoleNode[]): Group[] {
  const groups = new Map<string, Group>();
  for (const n of all) {
    const g: Group = groups.get(n.pool) || { pool: n.pool, nodes: [], hosts: new Map<string, number>(), host: null, fp: "", up: 0, sandboxes: 0, free: 0, label: "" };
    g.nodes.push(n);
    if (n.host) { const k = JSON.stringify(n.host); g.hosts.set(k, (g.hosts.get(k) || 0) + 1); }
    groups.set(n.pool, g);
  }
  const list = [...groups.values()];
  for (const g of list) {
    g.host = g.hosts.size ? (JSON.parse([...g.hosts].sort((a, b) => b[1] - a[1])[0][0]) as HostInfo) : null;
    g.fp = g.nodes[0].fingerprint || "";
    g.up = g.nodes.filter((n) => n.connected).length;
    g.sandboxes = g.nodes.reduce((a, n) => a + sandboxesOn(n), 0);
    g.free = g.nodes.filter((n) => n.connected).reduce((a, n) => a + n.free_mib, 0);
    g.nodes.sort((a, b) => a.id.localeCompare(b.id, undefined, { numeric: true }));
  }
  // Two groups can share a host description only when they asked for
  // different pools (from before groups were by host alone); say which.
  const seen = new Map<string, number>();
  for (const g of list) { g.label = hostLabel(g.host) || g.fp.slice(0, 16); seen.set(g.label, (seen.get(g.label) || 0) + 1); }
  for (const g of list) if ((seen.get(g.label) || 0) > 1) g.label = `${g.pool.split("#")[0]} · ${g.label}`;
  return list.sort((a, b) => b.nodes.length - a.nodes.length || a.label.localeCompare(b.label));
}

// The dropdowns: every host, and each group.
export function poolOptions(all: ConsoleNode[]): { value: string; label: string }[] {
  return [{ value: "", label: "All hosts" }, ...poolGroups(all).map((g) => ({ value: g.pool, label: g.label }))];
}
// The picked group if it is still there, else every host.
export const validPool = (all: ConsoleNode[], cur: string): string =>
  poolOptions(all).some((o) => o.value === cur) ? cur : "";
