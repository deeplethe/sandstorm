import { useEffect, useRef, useState } from "react";
import { st, useStore } from "../state";

// Delete asks twice, in the menu and in the bar: the first click arms it, and
// three seconds later it disarms on its own. `arm(key)` says whether this
// click is the second.
export function useArm(): [(key: string) => boolean, (key: string) => boolean, () => void] {
  const ref = useRef(new Set<string>());
  const [, set] = useState(0);
  const isArmed = (key: string) => ref.current.has(key);
  const arm = (key: string) => {
    if (ref.current.has(key)) return true;
    ref.current.add(key); set((n) => n + 1);
    setTimeout(() => { ref.current.delete(key); set((n) => n + 1); }, 3000);
    return false;
  };
  const reset = () => { if (ref.current.size) { ref.current.clear(); set((n) => n + 1); } };
  return [isArmed, arm, reset];
}

// Typing in a search field filters once it pauses, not on every key.
export function useDebounced(value: string, ms: number): string {
  const [v, setV] = useState(value);
  useEffect(() => { const t = setTimeout(() => setV(value), ms); return () => clearTimeout(t); }, [value, ms]);
  return v;
}

// Calls `load` when `tab` is shown and after every refresh while it shows,
// the way the tables that page on the master keep up.
export function useTabLoad(tab: string, load: (why: "show" | "refresh") => void): void {
  useStore();
  const shown = st.tab === tab;
  const ref = useRef(load);
  ref.current = load;
  useEffect(() => { if (shown) ref.current("show"); }, [shown]);
  const n = st.refreshed;
  useEffect(() => { if (n && st.tab === tab) ref.current("refresh"); }, [n, tab]);
}

// Escape closes a dialog while it is open.
export function useEscape(open: boolean, close: () => void): void {
  const ref = useRef(close);
  ref.current = close;
  useEffect(() => {
    if (!open) return;
    const k = (e: KeyboardEvent) => { if (e.key === "Escape") ref.current(); };
    addEventListener("keydown", k);
    return () => removeEventListener("keydown", k);
  }, [open]);
}
