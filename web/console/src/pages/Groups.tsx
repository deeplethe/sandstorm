// The Groups page: nodes grouped by host -- engine, kernel and CPU, which
// decide where a snapshot restores -- and a dialog with one group's host and
// its nodes.
import { useState } from "react";
import { fmt, gib } from "../format";
import { cpuName, engineName, poolGroups } from "../groups";
import { st } from "../state";
import { useDebounced } from "../components/hooks";
import { GroupDialog } from "../dialogs/GroupDialog";

export function Groups() {
  const [qIn, setQIn] = useState("");
  const q = useDebounced(qIn.trim().toLowerCase(), 150);
  const [open, setOpen] = useState("");
  const groups = poolGroups(st.NOW ? st.NOW.nodes : []);
  const hits = (g: (typeof groups)[number]) => (q ? g.nodes.filter((n) => n.id.toLowerCase().includes(q)).map((n) => n.id) : []);
  const shown = groups.filter((g) => !q || g.label.toLowerCase().includes(q) || hits(g).length);
  const openPool = (pool: string) => { if (groups.some((g) => g.pool === pool)) setOpen(pool); };

  return (
    <section data-tab="pools" className="card span-12" style={{ animationDelay: ".42s" }} hidden={st.tab !== "pools"}>
      <div className="head"><div><h2>Host groups</h2><div className="sub" id="poolsSub">{st.NOW ? (groups.length > 1
        ? `${fmt(groups.length)} groups. A snapshot only restores on hosts like the one it was taken on, so forks stay in their group; new sandboxes go to any.`
        : "Every node runs on the same kind of host, so a snapshot restores on any of them.") : ""}</div></div></div>
      <div className="bar"><input id="plQ" type="search" placeholder="Search by host or node" value={qIn} onChange={(e) => setQIn(e.target.value)} /></div>
      <div className="tbl-wrap">
        {st.NOW && (
          <table className="tbl pools-tbl" id="plTable">
            <thead><tr><th>CPU</th><th>Backend</th><th>Kernel</th><th className="r">Nodes</th><th className="r">Sandboxes</th><th className="r">Free</th></tr></thead>
            <tbody>
              {shown.length ? shown.map((g) => {
                const h = g.host, hs = hits(g);
                return (
                  <tr key={g.pool} tabIndex={0} title="Open this group" onClick={() => openPool(g.pool)}
                    onKeyDown={(e) => { if (e.key === "Enter" || e.key === " ") { e.preventDefault(); openPool(g.pool); } }}>
                    <td className="cpu"><div className="ndname" title={h ? h.cpu_model : ""}>{h ? cpuName(h.cpu_model) || h.cpu_vendor : "not reported"}</div>
                      <div className="fpx">{g.fp.slice(0, 16)}</div>{hs.length > 0 && <div className="hits">{hs.join(", ")}</div>}</td>
                    <td>{h ? engineName(h.engine) : ""}</td><td>{h ? h.kernel : ""}</td>
                    <td className="r num">{fmt(g.nodes.length)}{g.up < g.nodes.length && <> <span className="sub inline">{fmt(g.nodes.length - g.up)} silent</span></>}</td>
                    <td className="r num">{fmt(g.sandboxes)}</td>
                    <td className="r num">{gib(g.free)}</td>
                  </tr>
                );
              }) : <tr><td colSpan={7}><div className="empty">{groups.length ? "No host or node matches." : "No nodes yet. Use “+ Add a node” on Nodes."}</div></td></tr>}
            </tbody>
          </table>
        )}
      </div>
      <GroupDialog pool={open} groups={groups} onClose={() => setOpen("")} />
    </section>
  );
}
