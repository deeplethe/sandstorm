// The one hover card every chart, strip and row shares. It is built from
// elements and text rather than markup, so a node's id is shown as it is.
let tipEl: HTMLElement | null = null;
export function setTipEl(e: HTMLElement | null): void { tipEl = e; }

/** A row of the card: label, swatch colour, value. */
export type TipRow = [string, string, string];

export function showTip(e: MouseEvent, title: string, rows: TipRow[]): void {
  const tip = tipEl;
  if (!tip) return;
  tip.replaceChildren();
  const t = document.createElement("div");
  t.className = "t"; t.textContent = title;
  tip.appendChild(t);
  for (const [l, c, v] of rows) {
    const r = document.createElement("div"), s = document.createElement("span"), i = document.createElement("i"), b = document.createElement("b");
    r.className = "r"; i.className = "swatch"; i.style.background = c; b.className = "num"; b.textContent = v;
    s.append(i, " " + l);
    r.append(s, b);
    tip.appendChild(r);
  }
  const x = Math.min(innerWidth - 200, e.clientX + 14), y = Math.min(innerHeight - 20 - rows.length * 18, e.clientY + 14);
  tip.style.left = x + "px"; tip.style.top = y + "px"; tip.classList.add("on");
}
export function hideTip(): void { tipEl?.classList.remove("on"); }
