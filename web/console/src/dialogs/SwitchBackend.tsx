// ---- change a node's backend ---------------------------------------------------
// Only an empty node can: what it holds is in its backend's format and could
// never be restored on another. The node restarts onto the new engine, and
// its fingerprint takes it to another host group, which is said before anyone
// confirms. The node makes the same checks again and refuses for itself.
import { useEffect, useState, type ReactNode } from "react";
import { createPortal } from "react-dom";
import type { BackendSwitchView, ConsoleNode } from "../types";
import { api } from "../api";
import { BACKEND_LABEL, backendOf, switchBlocker, switchConsequence, type Backend } from "../backends";
import { refresh } from "../state";
import { useEscape } from "../components/hooks";

export function SwitchBackend({ node, onClose }: { node: ConsoleNode | null; onClose: () => void }) {
  const [err, setErr] = useState<ReactNode>(""), [ok, setOk] = useState(""), [busy, setBusy] = useState(false);
  useEscape(!!node, onClose);
  const id = node?.id;
  useEffect(() => { setErr(""); setOk(""); setBusy(false); }, [id]);

  const from = backendOf(node?.host);
  const to: Backend = from === "firecracker" ? "gvisor" : "firecracker";
  const blocked = node ? switchBlocker(node) : "";
  const go = async () => {
    if (!node) return;
    setErr(""); setBusy(true);
    try {
      const r = await api<BackendSwitchView>(`/v1/nodes/${encodeURIComponent(node.id)}/backend`, { backend: to });
      setOk(r.message);
      void refresh(false);
    } catch (e) {
      setErr(<><b>It did not switch.</b><details open><summary>Details</summary><code>{String((e as Error).message || e)}</code></details></>);
    } finally { setBusy(false); }
  };

  return createPortal(
    <div className="gate" id="swDlg" hidden={!node} onClick={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <form className="wide" autoComplete="off" onSubmit={(e) => e.preventDefault()}>
        <h1>Change backend</h1>
        <button className="x" type="button" aria-label="Close" onClick={onClose}>×</button>
        {node && !ok && (blocked
          ? <div className="warn">{blocked}</div>
          : <>
            <p>{switchConsequence(node, to)}</p>
            <button type="button" className="go" disabled={busy} onClick={() => void go()}>{busy ? "Switching…" : `Switch to ${BACKEND_LABEL[to]}`}</button>
          </>)}
        <div className="ok" hidden={!ok}>{ok}</div>
        <div className="err">{err}</div>
      </form>
    </div>,
    document.body,
  );
}
