import type { Tier } from "./types";
import { local } from "./storage";

export const TIERS: readonly Tier[] = ["hot", "frozen", "snapshot", "detached"];
export const TIER_LABEL: Record<Tier, string> = { hot: "Awake", frozen: "Frozen", snapshot: "On disk", detached: "Detached" };

// Colours come from the stylesheet's tokens, read when something is drawn,
// so a canvas or chart follows the theme like the rest of the page.
export const css = (v: string): string => getComputedStyle(document.documentElement).getPropertyValue(v).trim();
export const tierColor = (t: Tier): string => css("--" + t);

// The theme picked in this browser, if any, before the first paint.
export function applySavedTheme(): void {
  const t = local.get("sandstorm-console-theme");
  if (t) document.documentElement.dataset.theme = t;
}

// The other one from what shows now: picked once, the choice wins over the
// system's from then on.
export function toggleTheme(): void {
  const cur = document.documentElement.dataset.theme;
  const dark = cur ? cur === "dark" : matchMedia("(prefers-color-scheme: dark)").matches;
  document.documentElement.dataset.theme = dark ? "light" : "dark";
  local.set("sandstorm-console-theme", document.documentElement.dataset.theme);
}
