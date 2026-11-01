import { useState } from "react";
import { BRAND_SVG } from "../brand";
import { compact, fmt } from "../format";
import { poolOptions } from "../groups";
import { redrawAll, refresh, setPool, setRange, signOut, st } from "../state";
import { view } from "../storage";
import { TIERS, toggleTheme } from "../theme";
import { Dropdown } from "./Dropdown";

const RANGES: [number, string][] = [[900, "15m"], [3600, "1h"], [21600, "6h"], [86400, "24h"]];
const NAV: [string, string][] = [["overview", "Overview"], ["nodes", "Nodes"], ["pools", "Groups"], ["sandboxes", "Sandboxes"], ["snapshots", "Snapshots"], ["activity", "Activity"]];

export function Header() {
  const now = st.NOW, tab = st.tab;
  const [spin, setSpin] = useState(false);
  const fleetTotal = now ? TIERS.reduce((a, t) => a + (now.tiers[t] || 0), 0) : 0;
  const fleetUp = now ? now.nodes.filter((n) => n.connected).length : 0;
  // The counts beside the tabs, once the master has answered.
  const counts: Record<string, string> = now ? {
    nodes: now.nodes.length ? `${fleetUp}/${now.nodes.length}` : "",
    pools: now.nodes.length ? fmt(new Set(now.nodes.map((n) => n.pool)).size) : "",
    sandboxes: fleetTotal ? compact(fleetTotal) : "",
  } : {};
  const refreshNow = async () => {
    if (spin) return;
    setSpin(true);
    try { await refresh(false); } finally { setTimeout(() => setSpin(false), 400); }
  };
  return (
    <header>
      <a className="brand" href="#overview" aria-label="Sandstorm"><svg id="brandMark" viewBox="1 8 296 52" role="img" aria-hidden="true" dangerouslySetInnerHTML={{ __html: BRAND_SVG }}></svg></a>
      <nav className="tabs-nav" id="tabs" aria-label="Sections">
        <span className="ind still" id="tabInd" aria-hidden="true"></span>
        {NAV.map(([id, label]) => (
          <a key={id} href={"#" + id} data-go={id} aria-current={tab === id ? "page" : undefined}>
            {label}{id in { nodes: 1, pools: 1, sandboxes: 1 } && <>{" "}<span className={"n" + (id === "nodes" && now && fleetUp < now.nodes.length ? " bad" : "")}>{counts[id] || ""}</span></>}
          </a>
        ))}
      </nav>
      <div className="live" title={st.live.text}><span className={st.live.dot} id="dot"></span><span className="lt">{st.live.text}</span>
        <button className={"iconbtn refresh" + (spin ? " spin" : "")} type="button" title="Refresh now" aria-label="Refresh now" onClick={refreshNow}>
          <svg viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.6" strokeLinecap="round" strokeLinejoin="round" aria-hidden="true"><path d="M13.5 8a5.5 5.5 0 1 1-1.6-3.9" /><path d="M13.5 2.5v3h-3" /></svg>
        </button></div>
      <div className="spacer"></div>
      <Dropdown id="ovPool" ariaLabel="Host group shown on the overview" options={poolOptions(now ? now.nodes : [])} value={st.pool} onChange={setPool} />
      {/* The time range only shapes the charts. */}
      <div className="seg" id="range" role="group" aria-label="Time range" style={{ visibility: tab === "overview" || tab === "nodes" ? undefined : "hidden" }}>
        {RANGES.map(([s, l]) => <button key={s} data-s={s} className={view.range === s ? "on" : ""} onClick={() => setRange(s)}>{l}</button>)}
      </div>
      <button className="iconbtn gh" id="theme" type="button" title="Switch light / dark" aria-label="Switch light / dark" onClick={() => { toggleTheme(); redrawAll(); }}>
        <svg className="sun" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" aria-hidden="true"><circle cx="8" cy="8" r="3" /><path d="M8 1.5v1.3M8 13.2v1.3M1.5 8h1.3M13.2 8h1.3M3.4 3.4l.9.9M11.7 11.7l.9.9M3.4 12.6l.9-.9M11.7 4.3l.9-.9" /></svg>
        <svg className="moon" viewBox="0 0 16 16" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinejoin="round" aria-hidden="true"><path d="M13.6 9.9A5.8 5.8 0 0 1 6.1 2.4a5.8 5.8 0 1 0 7.5 7.5z" /></svg>
      </button>
      <a className="iconbtn refresh gh" href="https://github.com/deeplethe/sandstorm" target="_blank" rel="noopener" title="Sandstorm on GitHub" aria-label="Sandstorm on GitHub"><svg viewBox="0 0 16 16" fill="currentColor" aria-hidden="true"><path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27.68 0 1.36.09 2 .27 1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.013 8.013 0 0 0 16 8c0-4.42-3.58-8-8-8z" /></svg></a>
      <button className="iconbtn" id="signout" title="Forget the token" onClick={signOut}>Sign out</button>
    </header>
  );
}
