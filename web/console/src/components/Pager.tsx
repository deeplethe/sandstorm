// ---- pager ---------------------------------------------------------------------
// One pager for every table: where you are, the pages within reach, and how
// many rows a page holds.
import { useRef, type ReactNode } from "react";
import { fmt } from "../format";
import { PAGE_SIZES } from "../storage";
import { Dropdown } from "./Dropdown";

// The first and last page, the ones either side of this one, and gaps.
function pageList(cur: number, pages: number): number[] {
  const want = new Set([0, pages - 1, cur - 1, cur, cur + 1]);
  if (cur <= 3) for (let p = 1; p <= 4; p++) want.add(p);
  if (cur >= pages - 4) for (let p = pages - 5; p < pages - 1; p++) want.add(p);
  return [...want].filter((p) => p >= 0 && p < pages).sort((a, b) => a - b);
}

// A new page starts at its first row, not wherever the pager left you.
function toTableTop(node: HTMLElement | null): void {
  const card = node?.closest(".card");
  if (card && card.getBoundingClientRect().top < 0) card.scrollIntoView({ behavior: "smooth", block: "start" });
}

interface Props { offset: number; size: number; total: number; shown: number; sizes?: number[]; go: (offset: number, size: number) => void }

export function Pager({ offset, size, total, shown, sizes = PAGE_SIZES, go }: Props) {
  const ref = useRef<HTMLDivElement>(null);
  if (!total) return <div className="pager" ref={ref}></div>;
  const pages = Math.max(1, Math.ceil(total / size)), cur = Math.min(pages - 1, Math.floor(offset / size));
  const to = (p: number) => { go(p * size, size); toTableTop(ref.current); };
  const nums: ReactNode[] = [];
  let prev = -1;
  for (const p of pageList(cur, pages)) {
    if (p - prev > 1) nums.push(<span className="gap" key={"g" + p}>…</span>);
    nums.push(<button type="button" key={p} aria-current={p === cur ? "page" : undefined} aria-label={`Page ${p + 1}`} onClick={() => to(p)}>{fmt(p + 1)}</button>);
    prev = p;
  }
  return (
    <div className="pager" ref={ref}>
      <span className="count">{fmt(offset + 1)}–{fmt(Math.min(total, offset + shown))} of {fmt(total)}</span>
      <span className="pages">
        <button type="button" disabled={cur === 0} aria-label="Previous page" onClick={() => to(cur - 1)}>‹</button>
        {nums}
        <button type="button" disabled={cur >= pages - 1} aria-label="Next page" onClick={() => to(cur + 1)}>›</button>
      </span>
      <label className="size">
        <Dropdown ariaLabel="Rows a page" options={sizes.map((n) => ({ value: String(n), label: String(n) }))} value={String(size)}
          onChange={(v) => { go(0, Number(v)); toTableTop(ref.current); }} />
        {" "}a page
      </label>
    </div>
  );
}
