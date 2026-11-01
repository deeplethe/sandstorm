// ---- snapshots ----------------------------------------------------------------
import { useState } from "react";
import type { SnapshotView } from "../types";
import { api, apiDelete, auth } from "../api";
import { ago, mib } from "../format";
import { st } from "../state";
import { useArm, useTabLoad } from "../components/hooks";
import { BuildSnapshot } from "../dialogs/BuildSnapshot";

export function Snapshots() {
  const [snaps, setSnaps] = useState<SnapshotView[] | null>(null);
  // What the master said when a delete failed, under its row until the
  // table is next loaded.
  const [errs, setErrs] = useState<Record<string, string[]>>({});
  const [deleting, setDeleting] = useState<string | null>(null);
  const [isArmed, arm, disarm] = useArm();
  const [building, setBuilding] = useState(false);
  const [live, setLive] = useState(false);

  const load = async () => {
    if (!auth.token) return;
    try { setSnaps(await api<SnapshotView[]>("/v1/snapshots")); setErrs({}); }
    catch (e) { if (String((e as Error).message) !== "401") console.warn(e); }
  };
  useTabLoad("snapshots", () => void load());

  const del = async (tag: string) => {
    if (!arm(tag)) return;
    setDeleting(tag);
    try { await apiDelete("/v1/snapshots/" + encodeURIComponent(tag)); void load(); }
    catch (e) { disarm(); setErrs((x) => ({ ...x, [tag]: [...(x[tag] || []), String((e as Error).message || e)] })); }
    finally { setDeleting(null); }
  };

  const nowS = Date.now() / 1000, ss = snaps || [];
  return (
    <section data-tab="snapshots" className="card span-12" style={{ animationDelay: ".44s" }} hidden={st.tab !== "snapshots"}>
      <div className="head"><div><h2>Snapshots</h2><div className="sub" id="snapSub">{(ss.length ? `${ss.length} on ${new Set(ss.map((x) => x.node_id)).size} node(s). ` : "") + "Images new sandboxes fork from in milliseconds."}</div></div>
        <button className="btn" id="bdOpen" type="button" onClick={() => setBuilding(true)}>{live ? "Resume the build" : "+ Build a snapshot"}</button></div>
      <div className="tbl-wrap">
        {snaps && (ss.length ? (
          <table className="tbl" id="snapTable">
            <thead><tr><th>Tag</th><th>Node</th><th>Template</th><th>Branched from</th><th className="r">Size</th><th className="r">Created</th><th className="r"></th></tr></thead>
            <tbody>
              {ss.slice().sort((a, b) => b.created_at - a.created_at).map((x) => (
                <tr key={x.tag}>
                  <td><div className="id wrap">{x.tag}</div></td><td>{x.node_id}</td><td>{x.template || ""}</td>
                  <td className="id">{x.branched_from ? x.branched_from : "–"}</td>
                  <td className="r">{x.size_bytes != null ? mib(x.size_bytes) : "–"}</td><td className="r">{ago(Math.max(0, nowS - x.created_at))}</td>
                  <td className="r"><div className="acts"><button type="button" className={"danger" + (isArmed(x.tag) && deleting !== x.tag ? " armed" : "")} disabled={deleting === x.tag}
                    onClick={() => void del(x.tag)}>{isArmed(x.tag) && deleting !== x.tag ? "Sure?" : "Delete"}</button></div>
                    {(errs[x.tag] || []).map((m, i) => <div className="sub bad" key={i}>{m}</div>)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        ) : <table className="tbl" id="snapTable"><tbody><tr><td><div className="empty">None yet. Use “+ Build a snapshot”: start a sandbox, set it up, and save it.</div></td></tr></tbody></table>)}
      </div>
      <BuildSnapshot open={building} onClose={() => setBuilding(false)} onLive={setLive} onSaved={() => void load()} />
    </section>
  );
}
