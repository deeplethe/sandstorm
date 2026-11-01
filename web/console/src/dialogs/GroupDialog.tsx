// One group: its hosts and its nodes. What its host is is read when it
// opens; its nodes follow each refresh while it is open.
import { useEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { fmt, gib } from "../format";
import { cpuName, engineName, flagsOf, sandboxesOn, type Group } from "../groups";
import { useEscape } from "../components/hooks";

export function GroupDialog({ pool, groups, onClose }: { pool: string; groups: Group[]; onClose: () => void }) {
  const [q, setQ] = useState("");
  // The group as it was when opened, and as last seen: if it goes while the
  // dialog is up, the dialog keeps what it showed.
  const [opened, setOpened] = useState<Group | null>(null);
  const lastSeen = useRef<Group | null>(null);
  const g = groups.find((x) => x.pool === pool) || null;
  if (g) lastSeen.current = g;
  useEffect(() => { if (pool) { setQ(""); setOpened(groups.find((x) => x.pool === pool) || null); } else { setOpened(null); lastSeen.current = null; } }, [pool]);
  useEscape(!!pool, onClose);

  const head = opened, cur = lastSeen.current, h = head?.host || null, flags = flagsOf(h);
  const needle = q.trim().toLowerCase(), ns = cur ? cur.nodes.filter((n) => !needle || n.id.toLowerCase().includes(needle)) : [];
  return createPortal(
    <div className="gate" id="plDlg" hidden={!pool || !head} onClick={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <form className="wide" autoComplete="off" onSubmit={(e) => e.preventDefault()}>
        <div className="plhead"><h1>{head ? (h ? cpuName(h.cpu_model) || h.cpu_vendor : head.label) : ""}</h1></div>
        <button className="x" type="button" aria-label="Close" onClick={onClose}>×</button>
        <div className="fpline">{head ? (h ? `${engineName(h.engine)} · kernel ${h.kernel} · ` : "") + `fingerprint ${head.fp}` : ""}</div>
        <dl className="pldl">
          {h ? <>
            <dt>Backend</dt><dd>{`${engineName(h.engine)} (${h.engine})`}</dd>
            <dt>Kernel</dt><dd>{h.kernel}</dd>
            <dt>CPU</dt><dd>{[h.cpu_vendor, h.cpu_model].filter(Boolean).join(" · ") || "–"}</dd>
            <dt>Flags</dt><dd>{flags.length ? <details><summary>{fmt(flags.length)} CPU flags</summary><div>{flags.join(" ")}</div></details> : "–"}</dd>
          </> : <><dt>Host</dt><dd>Not reported yet: nodes say what their host is from this version on.</dd></>}
        </dl>
        <div className="plsec">
          <h2><span>{cur ? `${fmt(cur.nodes.length)} ${cur.nodes.length === 1 ? "node" : "nodes"}` : "Nodes"}</span><input type="search" placeholder="Search nodes" value={q} onChange={(e) => setQ(e.target.value)} /></h2>
          <div className="plnodes">
            {ns.length ? ns.map((n) => (
              <div className="r" key={n.id}><span className="ndname"><span className={"dot" + (n.connected ? "" : " dead")}></span>{n.id}</span>
                <span>{n.connected ? gib(n.free_mib) : "silent"}</span><span>{fmt(sandboxesOn(n))} sandboxes</span></div>
            )) : <div className="empty">No node matches.</div>}
          </div>
        </div>
      </form>
    </div>,
    document.body,
  );
}
