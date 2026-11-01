// ---- snapshot builder -------------------------------------------------------------
// A build sandbox, a terminal into it, and a branch of it saved under a name.
// Every step is a call to the public API, so this does exactly what a script
// would. A line typed in the terminal is one exec of `sh -c`: the directory
// carries over from line to line (the prompt shows it), variables do not, and
// nothing interactive can run. The build sandbox's id, token and directory
// are kept in this tab, so closing the dialog or reloading leaves it to come
// back to rather than running with nobody to finish it.
//
// The builder's steps await one call after another and read and change
// where it is between them, as the page's own fields once were. So its state
// lives in one mutable object, as it did, and each step asks for a render
// where it used to bring the page up to date (`builderStep`).
import { useEffect, useLayoutEffect, useReducer, useRef, type KeyboardEvent } from "react";
import { createPortal } from "react-dom";
import type { CreatedSandbox, ExecView, SnapshotView, TemplateEntry } from "../types";
import { api, ApiError, auth, deleteQuietly } from "../api";
import { backendName, placeBody, placeNote, placeOptions, templateLabel } from "../backends";
import { errorText, mib } from "../format";
import { st } from "../state";
import { session } from "../storage";
import { Dropdown, type Option } from "../components/Dropdown";
import { useArm, useEscape } from "../components/hooks";

interface Build {
  id: string | null; token: string | null; node: string | null; base: string; cwd: string;
  running: boolean; saved: SnapshotView | null; checked: boolean; from: "template" | "snapshot"; hist: string[]; at: number;
}
const BD_TIMEOUT = 1800;

function loadBuild(): Build {
  const bd: Build = { id: null, token: null, node: null, base: "", cwd: "", running: false, saved: null, checked: false, from: "template", hist: [], at: 0 };
  try { Object.assign(bd, JSON.parse(session.get("sandstorm-build") || "{}")); } catch { /* none kept */ }
  if (!String(bd.cwd || "").startsWith("/")) bd.cwd = "";
  if (!Array.isArray(bd.hist)) bd.hist = [];
  bd.at = bd.hist.length;
  return bd;
}
function keepBuild(bd: Build): void {
  if (bd.id) session.set("sandstorm-build", JSON.stringify({ id: bd.id, token: bd.token, node: bd.node, base: bd.base, cwd: bd.cwd, hist: bd.hist.slice(-200) }));
  else session.remove("sandstorm-build");
}
// Escapes a terminal would act on are noise in a page.
// the escapes are what it removes
const unansi = (t: string) => t.replace(/\x1b\[[0-9;?]*[ -/]*[@-~]/g, "").replace(/\x1b\][^\x07]*\x07/g, "").replace(/\r(?!\n)/g, "\n");
const shortCwd = (d: string) => (d ? d.replace(/^\/root(?=\/|$)/, "~") : "");
const prompt = (cwd: string) => (shortCwd(cwd) ? shortCwd(cwd) + " " : "") + "$";
// One line, in the directory the last one left. Where it ends up comes back
// after a separator no command prints, and is taken off the output.
const shq = (x: string) => "'" + x.replace(/'/g, "'\\''") + "'";
function tagProblem(tag: string): string {
  if (!tag) return "Give it a name.";
  if (tag.length > 128) return "At most 128 characters.";
  if (/[/\\\0]/.test(tag) || tag.includes("..") || tag.startsWith(".")) return "A plain name: no slashes, no “..”, and not starting with a dot.";
  return "";
}

interface Props { open: boolean; onClose: () => void; onLive: (live: boolean) => void; onSaved: () => void }

export function BuildSnapshot({ open, onClose, onLive, onSaved }: Props) {
  const bd = useRef<Build>(null as unknown as Build);
  if (!bd.current) bd.current = loadBuild();
  const b = bd.current;
  // What the start form shows: the bases to start from and the nodes.
  const ui = useRef({ baseOpts: [] as Option[], base: "", baseLabel: "Template", nodeOpts: [{ value: "", label: "Any node with room" }] as Option[], node: "", tagHint: "" });
  // Which backend each snapshot was taken on, from the node that baked it.
  const snapBackend = useRef(new Map<string, string>());
  const [, render] = useReducer((n: number) => n + 1, 0);
  const builderStep = render;
  const termEl = useRef<HTMLDivElement>(null), inEl = useRef<HTMLTextAreaElement>(null), tagEl = useRef<HTMLInputElement>(null);
  const cleanupEl = useRef<HTMLInputElement>(null), startEl = useRef<HTMLButtonElement>(null);
  const openRef = useRef(open); openRef.current = open;
  const focusIn = useRef(false);
  const [isArmed, arm, disarm] = useArm();

  const term = (node: HTMLElement) => { const t = termEl.current!; t.appendChild(node); t.scrollTop = t.scrollHeight; };
  const termLine = (cls: string, text: string) => { const d = document.createElement("div"); if (cls) d.className = cls; d.textContent = text; term(d); };
  const termClear = () => { termEl.current!.replaceChildren(); };

  async function execIn(args: string[], timeout: number): Promise<ExecView> {
    const r = await fetch(`/v1/sandboxes/${encodeURIComponent(b.id || "")}/exec`, {
      method: "POST",
      headers: { authorization: "Bearer " + auth.token, "x-sandstorm-sandbox-token": b.token || "", "content-type": "application/json" },
      body: JSON.stringify({ args, timeout_secs: timeout }),
    });
    const text = await r.text();
    let j: (ExecView & { error?: string }) | null = null; try { j = JSON.parse(text); } catch { /* not JSON */ }
    if (!r.ok) throw new ApiError((j && j.error) || text || String(r.status), r.status);
    return j as ExecView;
  }
  async function runLine(cmd: string): Promise<ExecView> {
    const script = (b.cwd ? `cd ${shq(b.cwd)} 2>/dev/null || cd /\n` : "") + `${cmd}\n__rc=$?\nprintf '\\036%s' "$(pwd)"\nexit $__rc`;
    const r = await execIn(["sh", "-c", script], BD_TIMEOUT);
    let out = r.stdout || "";
    const k = out.lastIndexOf("\x1e");
    if (k >= 0) { const d = out.slice(k + 1).trim(); if (d.startsWith("/")) b.cwd = d; out = out.slice(0, k); keepBuild(b); }
    return { ...r, stdout: out };
  }
  // Running, and answering commands. The master's idle timers may have put the
  // build sandbox away while it waited for you, so wake it if they have.
  async function bdReady(limit = 90): Promise<void> {
    const t0 = Date.now(); let touched = false, last = "";
    while (Date.now() - t0 < limit * 1000) {
      try {
        const r = await execIn(["sh", "-c", "pwd"], 10);
        // -1 is the node saying it could not run the command at all -- the
        // sandbox is not up there yet, or not any more -- not the command's
        // own answer. That is not ready: wake it and ask again.
        if (r.exit_code === -1) throw new ApiError((r.stderr || "").trim() || "the build sandbox did not run the command", 503);
        const d = (r.stdout || "").trim();
        if (!b.cwd && d.startsWith("/")) { b.cwd = d; keepBuild(b); }
        return;
      } catch (e) {
        const status = (e as ApiError).status;
        if (status === 404 || status === 403) throw e;
        last = errorText(e);
        if (!touched) { touched = true; termLine("dim", "Waking the build sandbox…"); await api(`/v1/sandboxes/${encodeURIComponent(b.id || "")}/touch`, {}).catch(() => {}); }
        await new Promise((r) => setTimeout(r, 1000));
      }
    }
    throw new Error(`It did not start answering within ${limit} s: ${last}`);
  }
  function bdBusy(on: boolean) { b.running = on; if (!on && b.id && openRef.current) focusIn.current = true; builderStep(); }

  async function builderOptions() {
    if (!auth.token) return;
    try {
      const all = st.NOW?.nodes || [];
      const snaps = b.from === "template" ? [] : (await api<SnapshotView[]>("/v1/snapshots")).sort((x, y) => y.created_at - x.created_at);
      snapBackend.current = new Map(snaps.map((x) => [x.tag, backendName(all.find((n) => n.id === x.node_id)?.host)]));
      const opts = b.from === "template"
        ? (await api<TemplateEntry[]>("/v1/templates")).map((t) => (typeof t === "string" ? t : t.name))
        : snaps.map((x) => x.tag);
      const u = ui.current, keep = u.base;
      u.baseLabel = b.from === "template" ? "Template" : "Snapshot";
      u.baseOpts = opts.length ? opts.map((o) => ({ value: o, label: b.from === "template" ? templateLabel(all, o) : `${o} · ${snapBackend.current.get(o)}` })) : [{ value: "", label: b.from === "template" ? "No templates registered" : "No snapshots yet" }];
      u.base = opts.includes(keep) ? keep : u.baseOpts[0].value;
      // A fork goes where its snapshot restores, so it picks a node at most.
      u.nodeOpts = placeOptions(all).filter((p) => b.from === "template" || !p.value.startsWith("backend:"));
      if (!u.nodeOpts.some((o) => o.value === u.node)) u.node = "";
      // A build left from before a reload: carry on with it if it is still there.
      if (b.id && !b.checked) {
        b.checked = true;
        try { await api(`/v1/sandboxes/${encodeURIComponent(b.id)}`); termLine("dim", `Carrying on with build sandbox ${b.id}.`); }
        catch (e) { if ((e as ApiError).status === 404) { termLine("dim", `Build sandbox ${b.id} is gone; start another.`); b.id = null; keepBuild(b); } }
      }
    } catch (e) { if (String((e as Error).message) !== "401") termLine("bad", errorText(e)); }
    builderStep();
  }

  // The Snapshots tab, shown, fills the start form; so does opening.
  const onTab = st.tab === "snapshots";
  useEffect(() => { if (onTab) void builderOptions(); }, [onTab]);
  useEffect(() => {
    if (!open) return;
    void builderOptions();
    const t = setTimeout(() => (b.id ? inEl.current : startEl.current)?.focus(), 30);
    return () => clearTimeout(t);
  }, [open]);
  // Closing leaves a build sandbox running, to come back to.
  useEscape(open, onClose);
  useEffect(() => { onLive(!!b.id); });
  useLayoutEffect(() => { if (focusIn.current && inEl.current && !inEl.current.disabled) { focusIn.current = false; inEl.current.focus(); } });

  const start = async () => {
    const from = b.from, base = ui.current.base;
    if (!base) { termLine("bad", from === "template" ? "Register a template first (POST /v1/templates)." : "There is no snapshot to start from yet."); builderStep(); return; }
    const body: { template?: string; snapshot_tag?: string; node_id?: string; backend?: string } = { ...(from === "template" ? { template: base } : { snapshot_tag: base }), ...placeBody(ui.current.node) };
    termClear(); b.saved = null; b.cwd = ""; if (tagEl.current) tagEl.current.value = "";
    bdBusy(true);
    termLine("dim", `Starting a build sandbox from ${from} ${base}…`);
    try {
      const r = await api<CreatedSandbox | CreatedSandbox[]>("/v1/sandboxes", body);
      const v = Array.isArray(r) ? r[0] : r;
      Object.assign(b, { id: v.id, token: v.token || null, node: v.node_id, base: `${from} ${base}`, checked: true }); keepBuild(b);
      termLine("dim", `Created ${v.id} on ${v.node_id}. Waiting for it to answer…`);
      await bdReady();
      termLine("ok", "Ready.");
      termLine("dim", "Each line runs in a fresh sh: the directory carries over, variables do not, and nothing interactive runs (no editors or prompts). Paste several lines to run them in order. ↑ ↓ for history, Ctrl+L to clear. Save it below when it is set up.");
    } catch (e) { termLine("bad", errorText(e)); }
    finally { bdBusy(false); }
  };

  // Run what was typed or pasted, a line at a time, stopping at the first that fails.
  const run = async (text: string) => {
    const lines = text.split("\n").map((l) => l.trimEnd()).filter((l) => l.trim() && !l.trim().startsWith("#"));
    if (!lines.length) return;
    for (const l of lines) if (b.hist[b.hist.length - 1] !== l) b.hist.push(l);
    b.at = b.hist.length; keepBuild(b);
    bdBusy(true);
    try {
      await bdReady();
      for (const cmd of lines) {
        const d = document.createElement("div"), ps = document.createElement("span");
        d.className = "cmd"; ps.className = "ps"; ps.textContent = shortCwd(b.cwd) ? shortCwd(b.cwd) + " $" : "$";
        d.append(ps, " " + cmd); term(d);
        const t0 = performance.now();
        const r = await runLine(cmd);
        const secs = (performance.now() - t0) / 1000;
        if (r.stdout) termLine("", unansi(r.stdout).replace(/\n$/, ""));
        if (r.stderr) termLine("err", unansi(r.stderr).replace(/\n$/, ""));
        if (r.exit_code !== 0) {
          termLine("bad", `exit ${r.exit_code} · ${secs.toFixed(1)} s`);
          if (lines.length > 1) termLine("dim", "Stopped there; what ran before it is still in the sandbox.");
          break;
        }
        if (secs >= 2) termLine("st", `${secs.toFixed(1)} s`);
        builderStep();
      }
    } catch (e) { termLine("bad", errorText(e)); }
    finally { bdBusy(false); }
  };

  const fitIn = () => { const t = inEl.current!; t.style.height = "auto"; t.style.height = Math.min(t.scrollHeight, 160) + "px"; };
  const onKey = (e: KeyboardEvent<HTMLTextAreaElement>) => {
    const t = e.currentTarget;
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); const v = t.value; t.value = ""; fitIn(); void run(v); return; }
    if (e.key === "l" && e.ctrlKey) { e.preventDefault(); termClear(); return; }
    const first = !t.value.slice(0, t.selectionStart).includes("\n"), lastLine = !t.value.slice(t.selectionEnd).includes("\n");
    if (e.key === "ArrowUp" && first && b.hist.length) { e.preventDefault(); b.at = Math.max(0, b.at - 1); t.value = b.hist[b.at] || ""; fitIn(); }
    else if (e.key === "ArrowDown" && lastLine && b.hist.length) { e.preventDefault(); b.at = Math.min(b.hist.length, b.at + 1); t.value = b.hist[b.at] || ""; fitIn(); }
  };

  const save = async () => {
    const tag = tagEl.current!.value.trim(), why = tagProblem(tag);
    if (why) { ui.current.tagHint = why; builderStep(); tagEl.current!.focus(); return; }
    bdBusy(true);
    try {
      await bdReady();
      termLine("dim", `Saving ${tag}. The sandbox pauses for a moment while it is checkpointed…`);
      const snap = await api<SnapshotView>(`/v1/sandboxes/${encodeURIComponent(b.id || "")}/branch`, { tag });
      b.saved = snap;
      ui.current.tagHint = "";
      termLine("ok", `Saved ${snap.tag} on ${snap.node_id}${snap.size_bytes != null ? ` · ${mib(snap.size_bytes)}` : ""}. A new sandbox forks from it with {"snapshot_tag": "${snap.tag}"}.`);
      if (cleanupEl.current!.checked) {
        const id = b.id || "";
        await deleteQuietly("/v1/sandboxes/" + encodeURIComponent(id));
        b.id = null; b.token = null; b.cwd = ""; keepBuild(b);
        termLine("dim", `Deleted the build sandbox ${id}.`);
      }
      onSaved();
    } catch (e) { termLine("bad", errorText(e)); }
    finally { bdBusy(false); }
  };
  const fork = async () => {
    if (!b.saved) return;
    bdBusy(true);
    try {
      const r = await api<CreatedSandbox | CreatedSandbox[]>("/v1/sandboxes", { snapshot_tag: b.saved.tag });
      const v = Array.isArray(r) ? r[0] : r;
      termLine("ok", `Forked ${v.id} from ${b.saved.tag} on ${v.node_id}. It is in the Sandboxes tab.`);
    } catch (e) { termLine("bad", errorText(e)); }
    finally { bdBusy(false); }
  };
  const again = () => { b.saved = null; termClear(); void builderOptions(); builderStep(); };
  const discard = async () => {
    if (!arm("discard")) return;
    disarm();
    bdBusy(true);
    try {
      await deleteQuietly("/v1/sandboxes/" + encodeURIComponent(b.id || ""));
      termLine("dim", `Deleted the build sandbox ${b.id}. Nothing was saved from it.`);
      b.id = null; b.token = null; b.cwd = ""; keepBuild(b);
      termClear();
    } catch (e) { termLine("bad", errorText(e)); }
    finally { bdBusy(false); }
  };
  const setFrom = (v: "template" | "snapshot") => {
    if (b.running) return;
    b.from = v; builderStep();
    void builderOptions();
  };

  // What shows, from where the build is: the start form before there is a
  // build sandbox, the terminal once there is (or while it has something to
  // say), and the save bar under it. The terminal's lines are written to it
  // directly, so whether it has any is read from it.
  const live = !!b.id, running = b.running, saved = b.saved;
  const termHas = !!termEl.current?.childElementCount;
  const u = ui.current;
  return createPortal(
    <div className="bdlay" id="bdDlg" hidden={!open} onClick={(e) => { if (e.target === e.currentTarget) onClose(); }}>
      <div className={"bdbox" + (!live && !saved && !termHas ? " idle" : "")} role="dialog" aria-modal="true" aria-labelledby="bdTitle">
        <div className="bdhead">
          <div className="bdtitle"><h1 id="bdTitle">Build a snapshot</h1>
            <div className="bdwho">{live ? <><code>{b.id}</code> on {b.node || "?"} · from {b.base}</> : saved ? `Saved ${saved.tag}` : ""}</div></div>
          <button className={"btn" + (isArmed("discard") ? " armed" : "")} type="button" hidden={!live} disabled={running} onClick={() => void discard()}>{isArmed("discard") ? "Click again to discard" : "Discard"}</button>
          <button className="bdx" type="button" aria-label="Close" onClick={onClose}>×</button>
        </div>
        <div className="bdstart" hidden={live || !!saved}>
          <p>Start a build sandbox, set it up in the terminal here, then save it. New sandboxes fork from the snapshot in milliseconds, with everything you installed already there.</p>
          <div className="bdform">
            <div><label>Start from</label><div className="seg" role="radiogroup" aria-label="Start from">
              {(["template", "snapshot"] as const).map((v) => (
                <button type="button" role="radio" key={v} data-v={v} className={b.from === v ? "on" : ""} aria-checked={b.from === v} disabled={running} onClick={() => setFrom(v)}>{v === "template" ? "Template" : "Snapshot"}</button>
              ))}</div></div>
            <div className="grow"><label htmlFor="bdBase">{u.baseLabel}</label><Dropdown id="bdBase" options={u.baseOpts} value={u.base} disabled={running} onChange={(v) => { u.base = v; builderStep(); }} /></div>
            <div className="grow"><label htmlFor="bdNode">Where</label><Dropdown id="bdNode" options={u.nodeOpts} value={u.node} disabled={running} onChange={(v) => { u.node = v; builderStep(); }} /></div>
            <button className="btn primary" type="button" ref={startEl} disabled={running} onClick={() => void start()}>Start</button>
          </div>
          {b.from === "template"
            ? (() => { const { note, warn } = placeNote(placeOptions(st.NOW?.nodes || []).find((p) => p.value === u.node), u.base); return <><div className="note">{note}</div><div className="warn">{warn}</div></>; })()
            : u.base && <div className="note">{`A fork runs on the backend its snapshot was taken on (${snapBackend.current.get(u.base)}); a node on another backend cannot restore it.`}</div>}
        </div>
        <div className="bdterm" hidden={!live && !saved && !termHas}>
          {/* Clicking anywhere in the output puts you back at the prompt, as a
              terminal does, unless you are selecting text to copy. */}
          <div className="term" ref={termEl} role="log" aria-live="polite" onMouseUp={() => { if (!String(getSelection()) && inEl.current && !inEl.current.disabled) inEl.current.focus(); }}></div>
          <div className="bdline" hidden={!live}><span className="ps">{prompt(b.cwd)}</span>
            <textarea ref={inEl} rows={1} spellCheck={false} autoComplete="off" autoCapitalize="off" aria-label="Command" disabled={!live || running} onInput={fitIn} onKeyDown={onKey}></textarea>
            <span className="bdspin" hidden={!running}></span></div>
        </div>
        <div className="bdfoot" hidden={!live && !saved}>
          <input ref={tagEl} type="text" placeholder="Snapshot name, e.g. python-data-v1" spellCheck={false} autoComplete="off" aria-label="Snapshot name"
            disabled={!live || running} hidden={!live && !!saved}
            onInput={() => { if (u.tagHint) { u.tagHint = ""; builderStep(); } }}
            onKeyDown={(e) => { if (e.key === "Enter") { e.preventDefault(); void save(); } }} />
          <label className="check" hidden={!live && !!saved}><input type="checkbox" ref={cleanupEl} defaultChecked disabled={!live || running} hidden={!live && !!saved} /> Delete the build sandbox once saved</label>
          <span className="spacer"></span>
          <button className="btn" type="button" hidden={!saved} disabled={running} onClick={() => void fork()}>Fork a sandbox from it</button>
          <button className="btn" type="button" hidden={live || !saved} onClick={again}>Start another</button>
          <button className="btn primary" type="button" disabled={!live || running} hidden={!live && !!saved} onClick={() => void save()}>Save snapshot</button>
          <div className="hint" hidden={!u.tagHint}>{u.tagHint}</div>
        </div>
      </div>
    </div>,
    document.body,
  );
}
