// The loading screen: the page under the header, until the master first
// answers and while a new time range comes in. Driven directly rather than
// through React state, because it fades: it has to be shown, laid out, and
// only then given the class that makes it opaque, or there is nothing to
// fade from.
let L: HTMLElement | null = null, msgEl: HTMLElement | null = null, loadT = 0;

export function setLoadingEls(box: HTMLElement | null, msg: HTMLElement | null): void {
  L = box; msgEl = msg;
}

export function loading(on: boolean, msg?: string, err = false): void {
  if (!L || !msgEl) return;
  const box = L;
  clearTimeout(loadT);
  if (on) {
    msgEl.textContent = msg || "Loading the fleet"; box.classList.toggle("err", err);
    box.hidden = false; void box.offsetWidth; box.classList.add("on");
  }
  else if (!box.hidden) { box.classList.remove("on"); loadT = window.setTimeout(() => { box.hidden = true; }, 360); }
}
