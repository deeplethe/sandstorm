// ---- numbers that count up ------------------------------------------------
import { useLayoutEffect, useRef } from "react";
import { ease, fmt } from "../format";

interface Shown { v: HTMLElement; d: HTMLElement; had: boolean; last: number | null }
const shown = new WeakMap<HTMLElement, Shown>();

// A count that moved since the last refresh shows by how much beside it, for
// a few seconds; rates and latencies move every refresh, so they leave it off.
// `suffix` is markup of our own (a <small> unit), never data.
export function tweenText(el: HTMLElement, to: number | null, digits = 0, suffix = "", delta = false): void {
  let s = shown.get(el);
  if (!s) {
    el.innerHTML = '<span class="nv"></span><span class="nd" aria-hidden="true"></span>';
    s = { v: el.firstChild as HTMLElement, d: el.lastChild as HTMLElement, had: false, last: 0 };
    shown.set(el, s);
  }
  const had = s.had, from = s.last ?? 0;
  s.had = true; s.last = to;
  const v = s.v;
  if (to == null || !isFinite(to)) { v.innerHTML = "–"; return; }
  const d = to - from;
  if (delta && had && from != null && isFinite(from) && Math.round(d) !== 0) {
    const n = s.d;
    n.textContent = (d > 0 ? "+" : "−") + fmt(Math.abs(d), digits);
    for (const x of [n, v]) { x.classList.remove("up", "down"); void x.offsetWidth; x.classList.add(d > 0 ? "up" : "down"); }
  }
  const t0 = performance.now(), dur = 900;
  const step = (now: number) => {
    const k = Math.min(1, (now - t0) / dur);
    v.innerHTML = fmt(from + (to - from) * ease(k), digits) + suffix;
    if (k < 1) requestAnimationFrame(step);
  };
  requestAnimationFrame(step);
}

// A figure that counts to `value` whenever it changes. `undefined` is "no
// answer yet", which leaves the placeholder the element started with.
export function useTween<T extends HTMLElement>(value: number | null | undefined, digits = 0, suffix = "", delta = false, initial = "0") {
  const ref = useRef<T>(null);
  useLayoutEffect(() => { if (ref.current && !shown.has(ref.current)) ref.current.textContent = initial; }, [initial]);
  useLayoutEffect(() => {
    if (ref.current && value !== undefined) tweenText(ref.current, value, digits, suffix, delta);
  }, [value, suffix, digits, delta]);
  return ref;
}
