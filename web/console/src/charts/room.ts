// ---- node headroom ------------------------------------------------------------
// Free memory on each node now, tightest first, on one scale with the floor
// marked; as many rows as the card holds, and the rest a link to Nodes.
import type { ConsoleNode } from "../types";
import { escapeHtml, fmt, gib } from "../format";
import { css, tierColor } from "../theme";
import { sandboxesOn } from "../groups";
import { hideTip, showTip } from "./tip";

export const roomOrder = (nodes: ConsoleNode[]): ConsoleNode[] =>
  [...nodes].sort((a, b) => (Number(b.connected) - Number(a.connected)) || a.free_mib - b.free_mib);

// Drawn as markup: how many rows fit is read from the box as it stands.
export function drawRoom(box: HTMLElement, all: ConsoleNode[], floor: number): void {
  const nodes = roomOrder(all);
  if (!nodes.length) { box.innerHTML = `<div class="empty">No nodes yet.</div>`; return; }
  const vmax = Math.max(2 * floor, ...nodes.map((n) => n.free_mib)) || 1;
  const fits = box.clientHeight > 40 ? Math.max(1, Math.floor((box.clientHeight + 2) / 26)) : 8;
  const shown = nodes.length > fits ? nodes.slice(0, fits - 1) : nodes;
  box.innerHTML = shown.map((n) => {
    const col = !n.connected ? "var(--axis)" : n.free_mib < floor ? "var(--bad)" : n.free_mib < 2 * floor ? "var(--warn)" : "color-mix(in srgb, var(--ink) 38%, transparent)";
    return `<div class="rrow${n.connected ? "" : " down"}" data-id="${escapeHtml(n.id)}">
      <span class="rid"><span class="dot${n.connected ? "" : " dead"}"></span>${escapeHtml(n.id)}</span>
      <span class="rbar"><i style="width:${Math.min(100, 100 * n.free_mib / vmax)}%;background:${col}"></i>${floor > 0 ? `<u style="left:${100 * floor / vmax}%"></u>` : ""}</span>
      <span class="rv">${n.connected ? gib(n.free_mib) : "silent"}</span></div>`;
  }).join("") + (shown.length < nodes.length ? `<a class="more" href="#nodes">${nodes.length - shown.length} more on Nodes →</a>` : "");
}

export function roomHover(e: MouseEvent, all: ConsoleNode[]): void {
  const row = (e.target as Element).closest<HTMLElement>(".rrow"); if (!row) return hideTip();
  const n = all.find((x) => x.id === row.dataset.id); if (!n) return hideTip();
  showTip(e, n.id, [["free", css("--ink-2"), fmt(n.free_mib) + " MiB"], ["sandboxes", css("--ink-2"), fmt(sandboxesOn(n))],
    ["awake", tierColor("hot"), fmt(n.tiers.hot || 0)], ["pressure", css("--ink-2"), fmt(n.psi, 1)]]);
}
