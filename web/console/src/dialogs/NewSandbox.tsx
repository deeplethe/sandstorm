// ---- new sandbox ----------------------------------------------------------------
// The master places it on a node with room, unless one is picked. Its token
// is shown once, here, because the API returns it only at creation. Every
// choice of where says its backend, and a template nothing there can start is
// said before it is sent, since the two backends read templates differently.
import { useEffect, useRef, useState, type FormEvent, type ReactNode } from "react";
import { createPortal } from "react-dom";
import type { CreatedSandbox, TemplateEntry } from "../types";
import { api } from "../api";
import { placeBody, placeNote, placeOptions, templateLabel } from "../backends";
import { st } from "../state";
import { Dropdown, type Option } from "../components/Dropdown";
import { useEscape } from "../components/hooks";

export function NewSandbox({ open, seq, internet: byDefault, onClose, onCreated }: { open: boolean; seq: number; internet: boolean; onClose: () => void; onCreated: () => void }) {
  const [tpls, setTpls] = useState<Option[]>([]), [tpl, setTpl] = useState("");
  const [where, setWhere] = useState("");
  const [port, setPort] = useState("");
  const [internet, setInternet] = useState("");
  const [err, setErr] = useState<ReactNode>("");
  const [out, setOut] = useState<{ ok: string; token: string } | null>(null);
  const [busy, setBusy] = useState(false);
  const [copy, setCopy] = useState("Copy");
  const copyT = useRef(0);
  useEscape(open, onClose);

  // Each opening starts clean and asks for the templates afresh.
  useEffect(() => {
    if (!seq) return;
    setErr(""); setOut(null);
    (async () => {
      try {
        const list = await api<TemplateEntry[]>("/v1/templates");
        const o = list.length ? list.map((t) => { const n = typeof t === "string" ? t : t.name; return { value: n, label: templateLabel(st.NOW?.nodes || [], n) }; }) : [{ value: "", label: "No templates registered" }];
        setTpls(o); setTpl(o[0].value); setWhere(""); setInternet("");
      } catch (e) { setErr(String((e as Error).message || e)); }
    })();
  }, [seq]);

  const places = placeOptions(st.NOW?.nodes || []);
  const { note, warn } = placeNote(places.find((p) => p.value === where), tpl);

  const submit = async (e: FormEvent) => {
    e.preventDefault();
    setErr(""); setOut(null);
    const body: { template: string; node_id?: string; backend?: string; port?: number; egress?: { internet: boolean } } = { template: tpl, ...placeBody(where) };
    if (!body.template) { setErr("Register a template first (POST /v1/templates)."); return; }
    if (port) body.port = Number(port);
    if (internet) body.egress = { internet: internet === "on" };
    setBusy(true);
    try {
      const r = await api<CreatedSandbox | CreatedSandbox[]>("/v1/sandboxes", body);
      const v = Array.isArray(r) ? r[0] : r;
      setOut({ ok: `Created ${v.id} on ${v.node_id}` + (v.gateway_port ? `, reachable on port ${v.gateway_port} of that node.` : "."), token: v.token || "(no token returned)" });
      onCreated();
    } catch (x) {
      setErr(<><b>The master did not create it.</b><details open><summary>Details</summary><code>{String((x as Error).message || x)}</code></details></>);
    } finally { setBusy(false); }
  };
  const copyToken = async () => {
    try { await navigator.clipboard.writeText(out?.token || ""); setCopy("Copied"); } catch { setCopy("Select and copy"); }
    clearTimeout(copyT.current);
    copyT.current = window.setTimeout(() => setCopy("Copy"), 2000);
  };

  return createPortal(
    <div className="gate" id="newDlg" hidden={!open} onClick={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <form className="wide" autoComplete="off" onSubmit={submit}>
        <h1>New sandbox</h1>
        <button className="x" type="button" aria-label="Close" onClick={onClose}>×</button>
        <p>The master places it on a node with room, unless you pick one.</p>
        <label htmlFor="newTpl">Template</label><Dropdown id="newTpl" options={tpls} value={tpl} onChange={setTpl} />
        <label htmlFor="newNode">Where</label><Dropdown id="newNode" options={places} value={where} onChange={setWhere} />
        <div className="note" hidden={!note}>{note}</div>
        <div className="warn" hidden={!warn}>{warn}</div>
        <label htmlFor="newPort">Port inside the sandbox</label><input id="newPort" type="number" min="1" max="65535" placeholder="the template's default" value={port} onChange={(e) => setPort(e.target.value)} />
        <label htmlFor="newInternet">Internet</label><Dropdown id="newInternet" options={[{ value: "", label: `The default (${byDefault ? "on" : "off"})` }, { value: "on", label: "On" }, { value: "off", label: "Off" }]} value={internet} onChange={setInternet} />
        <button type="submit" className="go" disabled={busy}>Create it</button>
        <div hidden={!out}>
          <div className="ok">{out?.ok}</div>
          <div className="note">Its token is shown once. Requests to the sandbox's gateway port carry it in <code>x-sandstorm-sandbox-token</code>.</div>
          <div className="secret"><code>{out?.token}</code><button type="button" onClick={copyToken}>{copy}</button></div>
        </div>
        <div className="err">{err}</div>
      </form>
    </div>,
    document.body,
  );
}
