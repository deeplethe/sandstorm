// ---- nodes ----------------------------------------------------------------
// Filtered and paged here: the master sends every node with each refresh,
// and even a large fleet's list is small next to its sandboxes.
import { useEffect, useLayoutEffect, useRef, useState } from "react";
import type { ConsoleNode } from "../types";
import { ago, clock, fmt, gib } from "../format";
import { backendName, BACKEND_LABEL, fromEngine, switchBlocker } from "../backends";
import { hostLabel, poolMatch, poolOptions, sandboxesOn } from "../groups";
import { st } from "../state";
import { floorMib, hasCeil, maxPsi } from "../series";
import { keepPageSize, local, pageSize } from "../storage";
import { TIERS, TIER_LABEL } from "../theme";
import { strips } from "../charts/heat";
import { Dropdown } from "../components/Dropdown";
import { Pager } from "../components/Pager";
import { useDebounced } from "../components/hooks";
import { AddNode } from "../dialogs/AddNode";
import { SwitchBackend } from "../dialogs/SwitchBackend";

const ND_STATES: [string, string][] = [["", "All"], ["up", "Answering"], ["down", "Silent"], ["pressure", "Under pressure"]];

function NodeRows({ all, nodes, onSwitch }: { all: ConsoleNode[]; nodes: ConsoleNode[]; onSwitch: (id: string) => void }) {
  const floor = floorMib(), ceil = maxPsi();
  return (
    <table className="tbl nodes-tbl" id="ndTable">
      <thead><tr><th>Node</th><th>Backend</th><th className="r">Free</th><th>Sandboxes</th><th className="psi">Pressure</th><th>Features</th><th className="r">Seen</th></tr></thead>
      <tbody>
        {nodes.length ? nodes.map((n) => {
          const scale = hasCeil() ? 2 * ceil : 16;
          const psiCol = hasCeil() && n.psi > ceil ? "var(--bad)" : hasCeil() && n.psi > ceil / 2 ? "var(--warn)" : "var(--frozen)";
          return (
            <tr className={n.connected ? "" : "down"} key={n.id}>
              <td><div className="ndname"><span className={"dot" + (n.connected ? (hasCeil() && n.psi > ceil ? " stale" : "") : " dead")}></span>{n.id}</div><div className="sub">{hostLabel(n.host) || (n.fingerprint || "").slice(0, 16)}</div></td>
              <td>{n.switching_to ? <span className="sub inline">restarting onto {BACKEND_LABEL[fromEngine(n.switching_to) || "gvisor"]}</span> : <>{backendName(n.host)} <button className="link" type="button" title={switchBlocker(n) || "Change this node's backend"} onClick={() => onSwitch(n.id)}>Change</button></>}</td>
              <td className={"r num " + (n.free_mib < floor ? "bad" : "")} title={`${fmt(n.free_mib)} MiB free`}>{gib(n.free_mib)}</td>
              <td><b className="num">{fmt(sandboxesOn(n))}</b> <span className="sub inline">{fmt(n.tiers.hot || 0)} awake</span>
                <div className="stack">{TIERS.map((t) => <i key={t} title={`${TIER_LABEL[t]}: ${fmt(n.tiers[t] || 0)}`} style={{ flexGrow: n.tiers[t] || 0, background: `var(--${t})` }}></i>)}</div></td>
              <td><div className="meter" title={`memory pressure ${fmt(n.psi, 1)} (10 s average)${hasCeil() ? ` against ${fmt(ceil, 1)}` : ""}`}><i style={{ width: `${Math.min(100, n.psi / scale * 100)}%`, background: psiCol }}></i>{hasCeil() && <u style={{ left: "50%" }}></u>}</div></td>
              <td><span className="feats">{n.features.map((f) => <span key={f}>{f}</span>)}</span></td>
              <td className="r when">{n.connected ? ago(n.last_seen_age) : `silent ${ago(n.last_seen_age).replace(" ago", "")}`}</td>
            </tr>
          );
        }) : <tr><td colSpan={7}><div className="empty">{all.length ? "No node matches." : "No nodes yet. Use “+ Add a node”."}</div></td></tr>}
      </tbody>
    </table>
  );
}

// The same nodes' free memory over the range, a strip each.
function NodeHistory({ all, nodes }: { all: ConsoleNode[]; nodes: ConsoleNode[] }) {
  const rows = useRef<HTMLDivElement>(null), legend = useRef<HTMLDivElement>(null);
  const S = st.S, ids = nodes.map((n) => n.id).join("\n");
  useLayoutEffect(() => {
    if (rows.current && legend.current && st.S) strips(rows.current, legend.current, all, st.S, floorMib());
    // drawn when the rows or the data change
  }, [st.draw.v, ids, S]);
  return (
    <div id="ndHist">
      <div className="heatlegend" ref={legend}></div>
      <div className="hrows" ref={rows}>
        {nodes.length ? nodes.map((n) => (
          <div className={"hrow" + (n.connected ? "" : " down")} key={n.id}>
            <span className="hid" title={n.id}><span className={"dot" + (n.connected ? "" : " dead")}></span>{n.id}</span>
            <canvas className="strip" data-id={n.id}></canvas>
          </div>
        )) : <div className="empty">{all.length ? "No node matches." : "No nodes yet. Use “+ Add a node”."}</div>}
      </div>
      <div className="haxis"><span>{S ? clock(S.since) : ""}</span><span>{S ? clock(S.now) : ""}</span></div>
    </div>
  );
}

export function Nodes() {
  const [qIn, setQIn] = useState("");
  const q = useDebounced(qIn.trim().toLowerCase(), 150);
  const [state, setState] = useState("");
  const [pool, setPool] = useState("");
  const [offset, setOffset] = useState(0);
  const [size, setSize] = useState(() => pageSize("nodes", 20));
  // A table of the nodes or their memory over time: one view at a time, on
  // the same search, filter and page, remembered in this browser.
  const [view, setView] = useState<"table" | "history">(() => (local.get("sandstorm-console-nodes-view") === "history" ? "history" : "table"));
  const [adding, setAdding] = useState(false);
  const [switching, setSwitching] = useState("");
  useEffect(() => setOffset(0), [q]);

  const all = st.NOW ? st.NOW.nodes : [];
  const up = all.filter((n) => n.connected).length, floor = floorMib(), ceil = maxPsi();
  const limits = [floor > 0 ? `under ${fmt(floor)} MiB free` : "", hasCeil() ? `over ${fmt(ceil, 1)} memory pressure` : ""].filter(Boolean);
  const opts = poolOptions(all);
  const pl = opts.some((o) => o.value === pool) ? pool : "";
  const match = all.filter((n) => poolMatch(pl, n.pool) && (!q || n.id.toLowerCase().includes(q))
    && (state === "" || (state === "up" ? n.connected : state === "down" ? !n.connected : n.connected && ((hasCeil() && n.psi > ceil / 2) || n.free_mib < floor))))
    .sort((a, b) => a.id.localeCompare(b.id, undefined, { numeric: true }));
  const off = offset >= match.length ? Math.max(0, Math.floor((match.length - 1) / size) * size) : offset;
  useEffect(() => { if (off !== offset) setOffset(off); }, [off, offset]);
  const nodes = match.slice(off, off + size);

  return (
    <section data-tab="nodes" className="card span-12" style={{ animationDelay: ".42s" }} hidden={st.tab !== "nodes"}>
      <div className="head"><div><h2>Nodes</h2><div className="sub" id="nodesSub">{st.NOW ? `${up} of ${all.length} answering.` + (limits.length ? ` The master holds back work from a node ${limits.join(" or ")}.` : "") : ""}</div></div>
        <button className="btn" id="addNode" type="button" onClick={() => setAdding(true)}>+ Add a node</button></div>
      <div className="bar">
        <input id="ndQ" type="search" placeholder="Search by node" value={qIn} onChange={(e) => setQIn(e.target.value)} />
        <Dropdown id="ndPool" ariaLabel="Host group" options={opts} value={pl} onChange={(v) => { setPool(v); setOffset(0); }} />
        <div className="chips" id="ndState">
          {ND_STATES.map(([v, l]) => <button type="button" key={v} data-v={v} aria-pressed={state === v} onClick={() => { setState(v); setOffset(0); }}>{l}</button>)}
        </div>
        <div className="seg" id="ndView" role="group" aria-label="View">
          {(["table", "history"] as const).map((v) => (
            <button type="button" key={v} data-v={v} className={view === v ? "on" : ""}
              onClick={() => { if (v === view) return; setView(v); local.set("sandstorm-console-nodes-view", v); }}>{v === "table" ? "Table" : "Memory over time"}</button>
          ))}
        </div>
      </div>
      {view === "table" ? <div className="tbl-wrap" id="ndTableWrap">{st.NOW && <NodeRows all={all} nodes={nodes} onSwitch={setSwitching} />}</div>
        : st.NOW && <NodeHistory all={all} nodes={nodes} />}
      <Pager offset={off} size={size} total={match.length} shown={nodes.length}
        go={(o, s) => { if (s !== size) { setSize(s); keepPageSize("nodes", s); } setOffset(o); }} />
      <AddNode open={adding} onClose={() => setAdding(false)} />
      <SwitchBackend node={all.find((n) => n.id === switching) || null} onClose={() => setSwitching("")} />
    </section>
  );
}
