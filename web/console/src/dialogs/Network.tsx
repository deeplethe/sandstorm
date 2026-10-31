// ---- what a sandbox may reach ---------------------------------------------------
// The same form for one sandbox and for the default new ones start from, which
// is why it is handed a path to post to. The master checks the entries; what
// it objects to is shown here as it said it.
import { useEffect, useState } from "react";
import { createPortal } from "react-dom";
import type { Egress } from "../types";
import { api } from "../api";
import { useEscape } from "../components/hooks";

export interface NetworkTarget { title: string; intro: string; path: string; egress: Egress }

const lines = (l?: string[]): string => (l || []).join("\n");
const entries = (t: string): string[] => t.split(/[\s,]+/).filter(Boolean);

export function Network({ target, onClose, onSaved }: { target: NetworkTarget | null; onClose: () => void; onSaved: (e: Egress) => void }) {
  const [internet, setInternet] = useState(false);
  const [allow, setAllow] = useState(""), [deny, setDeny] = useState("");
  const [err, setErr] = useState(""), [busy, setBusy] = useState(false);
  useEscape(!!target, onClose);
  useEffect(() => {
    setErr(""); setBusy(false);
    setInternet(!!target?.egress.internet); setAllow(lines(target?.egress.allow)); setDeny(lines(target?.egress.deny));
  }, [target]);

  const save = async () => {
    if (!target) return;
    setErr(""); setBusy(true);
    try {
      onSaved(await api<Egress>(target.path, { internet, allow: entries(allow), deny: entries(deny) }));
      onClose();
    } catch (e) { setErr(String((e as Error).message || e)); setBusy(false); }
  };

  return createPortal(
    <div className="gate" id="netDlg" hidden={!target} onClick={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <form className="wide" autoComplete="off" onSubmit={(e) => { e.preventDefault(); void save(); }}>
        <h1>{target?.title}</h1>
        <button className="x" type="button" aria-label="Close" onClick={onClose}>×</button>
        <p>{target?.intro}</p>
        <label className="check"><input id="netInternet" type="checkbox" checked={internet} onChange={(e) => setInternet(e.target.checked)} /> Reach the public internet</label>
        <div className="note">Never private addresses, other sandboxes or the cloud metadata service. Those need an entry under Always allow.</div>
        <label htmlFor="netAllow">Always allow</label>
        <textarea id="netAllow" rows={3} spellCheck={false} placeholder="8.8.8.8, 10.1.0.0/16" value={allow} onChange={(e) => setAllow(e.target.value)} />
        <label htmlFor="netDeny">Block, when the internet is on</label>
        <textarea id="netDeny" rows={3} spellCheck={false} placeholder="203.0.113.0/24" value={deny} onChange={(e) => setDeny(e.target.value)} />
        <div className="note">IPv4 addresses or CIDRs, one per line. Always allow wins over Block.</div>
        <button type="submit" className="go" disabled={busy}>Save</button>
        <div className="err">{err}</div>
      </form>
    </div>,
    document.body,
  );
}
