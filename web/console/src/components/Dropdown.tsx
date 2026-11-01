// ---- dropdowns --------------------------------------------------------------
// Every select wears our own button and list instead of the system's menu.
// The select stays underneath, hidden, so the stylesheet's rules that follow
// it (`#ovPool + .dd`) and labels that point at it still apply; the button
// says the choice and the list is in the menu's style.
import { useEffect, useLayoutEffect, useRef, useState, type KeyboardEvent } from "react";
import { createPortal } from "react-dom";

export interface Option { value: string; label: string; disabled?: boolean }

// One list open at a time: opening another closes this one.
let closeOpen: (() => void) | null = null;

interface Props {
  id?: string;
  options: Option[];
  value: string;
  onChange: (value: string) => void;
  ariaLabel?: string;
  disabled?: boolean;
}

export function Dropdown({ id, options, value, onChange, ariaLabel, disabled = false }: Props) {
  const btn = useRef<HTMLButtonElement>(null);
  const list = useRef<HTMLDivElement>(null);
  const [open, setOpen] = useState(false);
  const [name, setName] = useState<string | undefined>(ariaLabel);
  // A select with no option for its value shows its first one.
  const found = options.findIndex((o) => o.value === value);
  const cur = found >= 0 ? found : options.length ? 0 : -1;

  const close = (refocus = false) => {
    setOpen(false);
    if (closeOpen === closeThis) closeOpen = null;
    if (refocus) btn.current?.focus();
  };
  const closeThis = () => close();
  const show = () => {
    if (closeOpen) closeOpen();
    closeOpen = closeThis;
    setOpen(true);
  };
  const pick = (i: number) => {
    close(true);
    if (cur === i) return;
    onChange(options[i].value);
  };

  // Named as its select was: its aria-label, or the label that points at it.
  // Clicking that label goes to the button, the select being hidden.
  useEffect(() => {
    const label = id ? document.querySelector<HTMLLabelElement>(`label[for="${id}"]`) : null;
    setName(ariaLabel || label?.textContent || undefined);
    if (!label) return;
    const go = (e: MouseEvent) => { e.preventDefault(); btn.current?.focus(); };
    label.addEventListener("click", go);
    return () => label.removeEventListener("click", go);
  }, [id, ariaLabel]);

  // Placed under the button, or over it when there is more room above, and
  // the current choice focused.
  useLayoutEffect(() => {
    const l = list.current, b = btn.current;
    if (!open || !l || !b) return;
    const r = b.getBoundingClientRect();
    l.style.minWidth = r.width + "px";
    const h = l.offsetHeight, below = innerHeight - r.bottom - 8;
    l.style.left = Math.max(8, Math.min(r.left, innerWidth - l.offsetWidth - 8)) + "px";
    l.style.top = (h > below && r.top > below ? r.top - h - 4 : r.bottom + 4) + "px";
    (l.querySelector<HTMLButtonElement>('[aria-selected="true"]') || l.querySelector<HTMLButtonElement>("button"))?.focus();
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const l = list.current;
    // Listened for on the list itself, so stopping Escape here keeps it
    // from the window, where it would close the dialog under the list.
    const keys = (e: globalThis.KeyboardEvent) => {
      if (!l) return;
      const items = [...l.querySelectorAll<HTMLButtonElement>("button:not(:disabled)")], at = items.indexOf(document.activeElement as HTMLButtonElement);
      if (e.key === "ArrowDown" || e.key === "ArrowUp") {
        e.preventDefault();
        items[(at + (e.key === "ArrowDown" ? 1 : items.length - 1)) % items.length]?.focus();
      } else if (e.key === "Home" || e.key === "End") {
        e.preventDefault(); items[e.key === "Home" ? 0 : items.length - 1]?.focus();
      } else if (e.key === "Escape" || e.key === "Tab") {
        // Escape closes the list, not the dialog under it.
        e.preventDefault(); e.stopPropagation(); close(true);
      }
    };
    const down = (e: PointerEvent) => {
      const t = e.target as Node;
      if (!l?.contains(t) && !btn.current?.contains(t)) close();
    };
    const scroll = (e: Event) => { if (!l?.contains(e.target as Node)) close(); };
    const resize = () => close();
    l?.addEventListener("keydown", keys);
    document.addEventListener("pointerdown", down);
    addEventListener("scroll", scroll, true);
    addEventListener("resize", resize);
    return () => {
      l?.removeEventListener("keydown", keys);
      document.removeEventListener("pointerdown", down);
      removeEventListener("scroll", scroll, true);
      removeEventListener("resize", resize);
    };
    // bound for as long as the list is open
  }, [open]);

  const onKey = (e: KeyboardEvent<HTMLButtonElement>) => {
    if (e.key === "ArrowDown" || e.key === "ArrowUp") { e.preventDefault(); show(); }
  };

  return (
    <>
      <select id={id} data-dd="" aria-label={ariaLabel} value={cur >= 0 ? options[cur].value : ""} disabled={disabled} onChange={(e) => onChange(e.target.value)}>
        {options.map((o, i) => <option key={i} value={o.value} disabled={o.disabled}>{o.label}</option>)}
      </select>
      <button type="button" className="dd" ref={btn} aria-haspopup="listbox" aria-expanded={open} aria-label={name} disabled={disabled}
        onClick={() => (open ? close() : show())} onKeyDown={onKey}>
        <span>{cur >= 0 ? options[cur].label : ""}</span>
        <svg viewBox="0 0 10 6" aria-hidden="true"><path d="M1 1l4 4 4-4" /></svg>
      </button>
      {open && createPortal(
        <div className="menu ddl" role="listbox" ref={list}>
          {options.map((o, i) => (
            <button type="button" role="option" key={i} aria-selected={i === cur} disabled={o.disabled} onClick={() => pick(i)}>{o.label}</button>
          ))}
        </div>,
        document.body,
      )}
    </>
  );
}
