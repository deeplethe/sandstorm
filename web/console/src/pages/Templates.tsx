// ---- templates from images ------------------------------------------------------
// A template is otherwise files put on each node by hand, so only the ones
// pulled from an image are here. Every node pulls for itself, and a slow or
// failed one does not hold up the rest: a sandbox can start as soon as one
// node has it, on the nodes that do.
import { useEffect, useState } from "react";
import type { PullView } from "../types";
import { api, ApiError, auth } from "../api";
import { st } from "../state";
import { useTabLoad } from "../components/hooks";
import { FromImage } from "../dialogs/FromImage";

const STATE_LABEL = { pulling: "Pulling", ready: "Ready", failed: "Failed" };

export function Templates() {
  const [pulls, setPulls] = useState<PullView[] | null>(null);
  const [open, setOpen] = useState(false);
  const [err, setErr] = useState("");

  const load = async () => {
    if (!auth.token) return;
    try { setPulls(await api<PullView[]>("/v1/templates/pulls")); }
    catch (e) { if (String((e as Error).message) !== "401") setErr(String((e as Error).message || e)); }
  };
  useTabLoad("snapshots", () => void load());

  // While anything is on its way, keep looking: there is no event to wait on.
  const busy = (pulls || []).some((p) => !p.nodes.length || p.nodes.some((n) => n.state === "pulling"));
  useEffect(() => {
    if (!busy || st.tab !== "snapshots") return;
    const t = window.setInterval(() => void load(), 3000);
    return () => clearInterval(t);
  }, [busy]);

  const retry = async (p: PullView) => {
    setErr("");
    try { await api("/v1/templates/from-image", { name: p.name, image: p.image }); void load(); }
    catch (e) { setErr(e instanceof ApiError ? e.message : String(e)); }
  };

  const ps = pulls || [];
  return (
    <section data-tab="snapshots" className="card span-12" style={{ animationDelay: ".5s" }} hidden={st.tab !== "snapshots"}>
      <div className="head"><div><h2>Templates from images</h2><div className="sub">Each node pulls the image from its registry, with no Docker needed.</div></div>
        <button className="btn" id="tplOpen" type="button" onClick={() => setOpen(true)}>+ From an image</button></div>
      <div className="flash" style={{ color: "var(--bad)" }}>{err}</div>
      <div className="tbl-wrap">
        {pulls && (ps.length ? (
          <table className="tbl" id="tplTable">
            <thead><tr><th>Template</th><th>Image</th><th>Nodes</th><th className="r"></th></tr></thead>
            <tbody>
              {ps.map((p) => {
                const failed = p.nodes.some((n) => n.state === "failed");
                return (
                  <tr key={p.name}>
                    <td><div className="id wrap">{p.name}</div></td>
                    <td className="id">{p.image}</td>
                    <td>{p.nodes.length ? p.nodes.map((n) => (
                      <div key={n.node_id}>{n.node_id}: {STATE_LABEL[n.state]}{n.error && <span className="err1" title={n.error}>{n.error}</span>}</div>
                    )) : <span className="sub">Waiting for a node</span>}</td>
                    <td className="r">{failed && <button type="button" className="btn" onClick={() => void retry(p)}>Try again</button>}</td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        ) : <table className="tbl"><tbody><tr><td><div className="empty">None yet. Use “+ From an image”.</div></td></tr></tbody></table>)}
      </div>
      <FromImage open={open} onClose={() => setOpen(false)} onStarted={() => void load()} />
    </section>
  );
}
