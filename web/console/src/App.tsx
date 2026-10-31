import { useEffect, useLayoutEffect, useRef } from "react";
import { auth } from "./api";
import { setLoadingEls } from "./loading";
import { redrawAll, refresh, setLive, setTab, showGate, signIn, st, useStore } from "./state";
import { setTipEl } from "./charts/tip";
import { Header } from "./components/Header";
import { Overview } from "./pages/Overview";
import { Nodes } from "./pages/Nodes";
import { Groups } from "./pages/Groups";
import { Sandboxes } from "./pages/Sandboxes";
import { Snapshots } from "./pages/Snapshots";
import { Templates } from "./pages/Templates";
import { Activity } from "./pages/Activity";

function placeTabInd(): void {
  const a = document.querySelector<HTMLElement>(`#tabs a[data-go="${st.tab}"]`), ind = document.getElementById("tabInd");
  if (!a || !ind) return;
  ind.style.width = a.offsetWidth + "px";
  ind.style.transform = `translateX(${a.offsetLeft}px)`;
}
const setHead = () => document.documentElement.style.setProperty("--head", (document.querySelector("header")?.offsetHeight || 64) + "px");

function Loading() {
  const box = useRef<HTMLDivElement>(null), msg = useRef<HTMLDivElement>(null), mark = useRef<SVGSVGElement>(null);
  useLayoutEffect(() => {
    if (msg.current) msg.current.textContent = "Loading the fleet";
    setLoadingEls(box.current, msg.current);
    // The mark's grains swirl as they do in the logo: the header's mark,
    // each arm's grains a step behind the last.
    const src = document.querySelector("#brandMark .mark"), svg = mark.current;
    if (!src || !svg || svg.childElementCount) return;
    const g = src.cloneNode(true) as SVGGElement;
    g.removeAttribute("transform"); svg.appendChild(g);
    [...g.children].forEach((r, i, all) => { (r as SVGElement).style.animationDelay = `${-(i % (all.length / 3)) * 0.09}s`; });
    const b = g.getBBox(); svg.setAttribute("viewBox", `${b.x - 2} ${b.y - 2} ${b.width + 4} ${b.height + 4}`);
  }, []);
  return (
    <div className="loading on" id="loading" role="status" aria-live="polite" ref={box}>
      <div className="lbox"><svg id="loadMark" aria-hidden="true" ref={mark}></svg><div className="lmsg" ref={msg}></div></div>
    </div>
  );
}

function Gate() {
  const input = useRef<HTMLInputElement>(null);
  const { open, msg, seq } = st.gate;
  useEffect(() => { if (open) { const t = setTimeout(() => input.current?.focus(), 50); return () => clearTimeout(t); } }, [open, seq]);
  return (
    <div className="gate" id="gate" hidden={!open}>
      <form onSubmit={(e) => { e.preventDefault(); signIn(input.current?.value.trim() || ""); }}>
        <h1>Sandstorm console</h1>
        <p>Paste the master's API token. It is kept in this tab only and forgotten when the tab closes. On the master it is in <code>/var/lib/sandstorm/pki/api-token</code>.</p>
        <input id="tokenIn" ref={input} type="password" autoComplete="off" placeholder="API token" autoFocus />
        <button type="submit">Open the console</button>
        <div className="err">{msg}</div>
      </form>
    </div>
  );
}

export function App() {
  useStore();
  const tab = st.tab;

  // Once, as the page opens: the hash, the polling, the clock on the live
  // dot, and the redraws that a resize or the system's theme call for.
  useEffect(() => {
    const onHash = () => setTab(location.hash.slice(1));
    addEventListener("hashchange", onHash);
    // Canvases and charts take their colours when drawn, so a change of the
    // system's theme redraws them rather than waiting for the next refresh.
    const mq = matchMedia("(prefers-color-scheme: dark)"), onScheme = () => redrawAll();
    mq.addEventListener("change", onScheme);
    const onResize = () => { setHead(); placeTabInd(); redrawAll(); };
    addEventListener("resize", onResize);
    // The cards rise in once, when the page opens; switching tabs just shows them.
    const settle = setTimeout(() => document.body.classList.add("settled"), 1500);
    if (!auth.token) showGate(); else void refresh(true);
    const poll = setInterval(() => void refresh(false), 5000);
    const tick = setInterval(() => st.lastOk && setLive(), 1000);
    return () => {
      removeEventListener("hashchange", onHash); mq.removeEventListener("change", onScheme); removeEventListener("resize", onResize);
      clearTimeout(settle); clearInterval(poll); clearInterval(tick);
    };
  }, []);

  // A tab shown: the body says which (the stylesheet lays the overview out
  // on one screen), the indicator moves under it, and the charts are drawn
  // again, since they measure their box, which is nothing while hidden.
  useLayoutEffect(() => {
    for (const x of ["overview", "nodes", "pools", "sandboxes", "snapshots", "activity"]) document.body.classList.toggle("tab-" + x, x === tab);
    setHead();
    placeTabInd();
    const r = requestAnimationFrame(() => redrawAll());
    return () => cancelAnimationFrame(r);
  }, [tab]);
  // The counts beside the tabs change their width.
  useLayoutEffect(() => { placeTabInd(); });

  return (
    <>
      <Header />
      <Loading />
      <main>
        <Overview />
        <Nodes />
        <Groups />
        <Sandboxes />
        <Snapshots />
          <Templates />
        <Activity />
      </main>
      <div className="tip" id="tip" ref={setTipEl}></div>
      <Gate />
    </>
  );
}
