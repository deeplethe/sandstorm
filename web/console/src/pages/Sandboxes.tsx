// ---- sandboxes ----------------------------------------------------------------
// A page of the fleet's sandboxes, filtered on the master: on a fleet of
// 20,000 the whole list is about 10 MB, and the table only shows a page.
import { Fragment, useEffect, useLayoutEffect, useRef, useState, type MouseEvent as ReactMouseEvent } from "react";
import type { Egress, SandboxView, Tier } from "../types";
import { api, apiDelete, apiPage, auth } from "../api";
import { ago, compact, fmt } from "../format";
import { st } from "../state";
import { keepPageSize, pageSize } from "../storage";
import { TIERS, TIER_LABEL } from "../theme";
import { Dropdown } from "../components/Dropdown";
import { Pager } from "../components/Pager";
import { useArm, useDebounced, useEscape, useTabLoad } from "../components/hooks";
import { Network, type NetworkTarget } from "../dialogs/Network";
import { NewSandbox } from "../dialogs/NewSandbox";

type Act = "hot" | "frozen" | "snapshot" | "retry" | "net-on" | "net-off" | "delete";
const SB_CHIPS: [string, string][] = [["", "All"], ...TIERS.map((t): [string, string] => [t, TIER_LABEL[t]]), ["failing", "Failing"]];
const ACT_DONE: Record<Act, string> = { hot: "Waking", frozen: "Freezing", snapshot: "Putting to disk", retry: "Retrying", "net-on": "Internet on for", "net-off": "Internet off for", delete: "Deleting" };
const BULK: [Act, string][] = [["hot", "Wake"], ["frozen", "Freeze"], ["snapshot", "To disk"], ["retry", "Retry"], ["net-on", "Internet on"], ["net-off", "Internet off"], ["delete", "Delete"]];

// What can be done to a sandbox in the tier it is in. One list, for the row
// menu and, across a selection, for the bar above the table.
function sbActions(b: SandboxView): [Act, string][] {
  const a: [Act, string][] = [];
  if (b.tier !== "hot") a.push(["hot", "Wake"]);
  if (b.tier === "hot") a.push(["frozen", "Freeze"]);
  if (b.tier === "hot" || b.tier === "frozen") a.push(["snapshot", "Put to disk"]);
  if (b.last_error) a.push(["retry", "Retry now"]);
  a.push(b.egress.internet ? ["net-off", "Turn internet off"] : ["net-on", "Turn internet on"]);
  a.push(["delete", "Delete"]);
  return a;
}
// One action on one sandbox.
async function sbDo(id: string, act: Act): Promise<void> {
  const path = "/v1/sandboxes/" + encodeURIComponent(id);
  if (act === "delete") await apiDelete(path);
  else if (act === "retry") await api(path + "/retry", {});
  else if (act === "net-on" || act === "net-off") await api(path + "/egress", { internet: act === "net-on" });
  else await api(path + "/tier", { tier: act });
}
function TierPill({ t }: { t: Tier }) {
  return <span className="tierpill"><i style={{ background: `var(--${t})` }}></i>{TIER_LABEL[t] || t}</span>;
}

// What the table says about a sandbox's network.
function reach(e: Egress): string {
  const n = e.allow?.length || 0;
  return (e.internet ? "On" : "Off") + (n ? ` + ${n}` : "");
}

export function Sandboxes() {
  const [tier, setTier] = useState("");
  const [qIn, setQIn] = useState("");
  const q = useDebounced(qIn.trim(), 250);
  const [node, setNode] = useState("");
  const [offset, setOffset] = useState(0);
  const [size, setSize] = useState(() => pageSize("sandboxes", 20));
  const [page, setPage] = useState<{ total: number; rows: SandboxView[] }>({ total: 0, rows: [] });
  const [sel, setSel] = useState<Set<string>>(() => new Set());
  const [flashMsg, setFlash] = useState<{ msg: string; bad: boolean } | null>(null);
  const [bulkBusy, setBulkBusy] = useState(false);
  const [menu, setMenu] = useState<{ id: string; at: HTMLElement } | null>(null);
  const [newOpen, setNewOpen] = useState(false), [newSeq, setNewSeq] = useState(0);
  const [byDefault, setDefault] = useState<Egress | null>(null);
  const [network, setNetwork] = useState<NetworkTarget | null>(null);
  const [isArmed, arm, disarm] = useArm();
  const busy = useRef(false), flashT = useRef(0), menuEl = useRef<HTMLDivElement>(null);
  const want = useRef({ tier, q, node, offset, size });
  want.current = { tier, q, node, offset, size };
  const loaded = useRef(false);

  const flash = (msg: string, bad = false) => {
    setFlash({ msg, bad });
    clearTimeout(flashT.current); flashT.current = window.setTimeout(() => setFlash(null), 5000);
  };
  const load = async (): Promise<void> => {
    if (!auth.token || busy.current) return;
    busy.current = true;
    try {
      const w = want.current;
      const qs = new URLSearchParams({ limit: String(w.size), offset: String(w.offset) });
      if (w.tier === "failing") qs.set("failing", "true"); else if (w.tier) qs.set("tier", w.tier);
      if (w.q) qs.set("q", w.q);
      if (w.node) qs.set("node", w.node);
      const { total, rows } = await apiPage<SandboxView>("/v1/sandboxes?" + qs);
      if (w.offset >= total && total > 0) { busy.current = false; setOffset(Math.floor((total - 1) / w.size) * w.size); return; }
      loaded.current = true;
      setPage({ total, rows });
      setDefault(await api<Egress>("/v1/egress"));
    } catch (e) { if (String((e as Error).message) !== "401") flash(String((e as Error).message || e), true); }
    finally { busy.current = false; }
  };
  useTabLoad("sandboxes", () => void load());
  // A new filter or page: the first page of it, and nothing selected.
  const first = useRef(true);
  useEffect(() => {
    if (first.current) { first.current = false; return; }
    if (st.tab === "sandboxes") void load();
    // loads on what is asked for
  }, [tier, q, node, offset, size]);
  useEffect(() => { setOffset(0); setSel(new Set()); }, [q]);

  // The row menu.
  const closeMenu = () => setMenu(null);
  useEscape(!!menu, closeMenu);
  useEffect(() => {
    if (!menu) return;
    const click = (e: MouseEvent) => { if (!(e.target as Element).closest("#sbMenu")) closeMenu(); };
    addEventListener("click", click);
    addEventListener("scroll", closeMenu, { passive: true });
    return () => { removeEventListener("click", click); removeEventListener("scroll", closeMenu); };
  }, [menu]);
  useLayoutEffect(() => {
    const m = menuEl.current;
    if (!menu || !m) return;
    const r = menu.at.getBoundingClientRect(), b = m.getBoundingClientRect();
    m.style.left = Math.max(8, r.right - b.width) + "px";
    m.style.top = (r.bottom + b.height + 8 > innerHeight ? r.top - b.height - 4 : r.bottom + 4) + "px";
  }, [menu]);
  const openMenu = (e: ReactMouseEvent<HTMLButtonElement>, id: string) => {
    e.stopPropagation();
    if (menu && menu.id === id) return closeMenu();
    disarm();
    setMenu(page.rows.some((x) => x.id === id) ? { id, at: e.currentTarget } : null);
  };
  const menuAct = async (e: ReactMouseEvent, act: Act) => {
    e.stopPropagation();
    if (!menu) return;
    const id = menu.id;
    if (act === "delete" && !arm("menu")) return;
    closeMenu();
    try {
      await sbDo(id, act); flash(`${ACT_DONE[act]} ${id}`);
      if (act === "delete") setSel((s) => { const n = new Set(s); n.delete(id); return n; });
      void load();
    } catch (err) { flash(String((err as Error).message || err), true); }
  };

  // The bar over a selection: the same actions, to every selected sandbox
  // they apply to, four at a time.
  const bulk = async (act: Act) => {
    if (act === "delete" && !arm("bulk")) return;
    const ids = [...sel];
    const byId = new Map(page.rows.map((b) => [b.id, b]));
    // Only where it applies, judged on what this page knows; the rest go to
    // the master, which says no to what it cannot do.
    const todo = ids.filter((id) => !byId.has(id) || sbActions(byId.get(id)!).some(([a]) => a === act));
    const skipped = ids.length - todo.length;
    setBulkBusy(true);
    let ok = 0; const failed: string[] = [];
    const queue = todo.slice();
    await Promise.all([0, 1, 2, 3].map(async () => {
      while (queue.length) {
        const id = queue.shift()!;
        try { await sbDo(id, act); ok++; } catch (err) { failed.push(`${id}: ${(err as Error).message || err}`); }
      }
    }));
    setBulkBusy(false);
    if (act === "delete") setSel((s) => { const n = new Set(s); for (const id of todo) n.delete(id); return n; });
    flash(`${ACT_DONE[act]} ${ok}` + (skipped ? `, ${skipped} skipped (not applicable)` : "") + (failed.length ? `, ${failed.length} failed: ${failed[0]}` : ""), failed.length > 0);
    void load();
  };

  const nowS = Date.now() / 1000, rows = page.rows;
  const allSel = rows.length > 0 && rows.every((b) => sel.has(b.id));
  const toggle = (id: string, on: boolean) => setSel((s) => { const n = new Set(s); if (on) n.add(id); else n.delete(id); return n; });
  const nodeOpts = [{ value: "", label: "All nodes" }, ...(st.NOW ? st.NOW.nodes.map((n) => ({ value: n.id, label: n.id })) : [])];
  const failing = st.NOW ? st.NOW.failed : 0;
  const menuRow = menu ? rows.find((x) => x.id === menu.id) : undefined;

  return (
    <section data-tab="sandboxes" className="card span-12" style={{ animationDelay: ".44s" }} hidden={st.tab !== "sandboxes"}>
      <div className="head"><div><h2>Sandboxes</h2><div className="sub" id="sbSub">Every sandbox, a page at a time.</div></div>
        <div className="btns">
          <button className="btn" id="sbDefault" type="button" disabled={!byDefault} title="What new sandboxes may reach, unless they say otherwise"
            onClick={() => byDefault && setNetwork({ title: "New sandboxes", intro: "What a sandbox may reach unless it says otherwise. Sandboxes already running keep what they have.", path: "/v1/egress", egress: byDefault })}>Internet: {byDefault ? (byDefault.internet ? "on" : "off") : "…"} by default</button>
          <button className="btn" id="newSb" type="button" onClick={() => { setNewOpen(true); setNewSeq((n) => n + 1); }}>+ New sandbox</button>
        </div></div>
      <div className="bar">
        <input id="sbQ" type="search" placeholder="Search by id or template" value={qIn} onChange={(e) => setQIn(e.target.value)} />
        <div className="chips" id="sbTiers">
          {/* Sandboxes that will not start are in the table under the Failing
              filter, each with its error; the filter carries how many. */}
          {SB_CHIPS.map(([t, l]) => (
            <button type="button" key={t} data-t={t} aria-pressed={tier === t} onClick={() => { setTier(t); setOffset(0); setSel(new Set()); }}>
              {t && t !== "failing" && <i className="swatch" style={{ background: `var(--${t})` }}></i>}{l}{t === "failing" && failing > 0 && <> <span className="cnt">{compact(failing)}</span></>}
            </button>
          ))}
        </div>
        <Dropdown id="sbNode" options={nodeOpts} value={node} onChange={(v) => { setNode(v); setOffset(0); setSel(new Set()); }} />
        <span className="flash" style={{ color: flashMsg?.bad ? "var(--bad)" : "var(--good)" }}>{flashMsg?.msg}</span>
      </div>
      <div className="selbar" hidden={sel.size === 0}>
        <b>{fmt(sel.size)} selected</b>
        {BULK.map(([act, label]) => (
          <button type="button" key={act} disabled={bulkBusy} className={act === "delete" ? "danger" + (isArmed("bulk") ? " armed" : "") : undefined} onClick={() => void bulk(act)}>
            {act === "delete" && isArmed("bulk") ? "Click again to delete" : label}
          </button>
        ))}
        <button type="button" className="link" disabled={bulkBusy} onClick={() => setSel(new Set())}>Clear</button>
      </div>
      <div className="tbl-wrap">
        {loaded.current && (
          <table className="tbl" id="sbTable">
            <thead><tr><th className="ck"><input type="checkbox" aria-label="Select this page" checked={allSel}
              onChange={(e) => { const on = e.target.checked; setSel((s) => { const n = new Set(s); for (const b of rows) if (on) n.add(b.id); else n.delete(b.id); return n; }); }} /></th>
              <th>Sandbox</th><th>Template</th><th>Node</th><th>Tier</th><th>Reachable on</th><th>Internet</th><th className="r">Last active</th><th className="r"></th></tr></thead>
            <tbody>
              {rows.length ? rows.map((b) => (
                <tr key={b.id} className={sel.has(b.id) ? "sel" : undefined}>
                  <td className="ck"><input type="checkbox" aria-label={`Select ${b.id}`} checked={sel.has(b.id)} onChange={(e) => toggle(b.id, e.target.checked)} /></td>
                  <td><span className="id" title={b.id}>{b.id.length > 19 ? b.id.slice(0, 18) + "…" : b.id}</span></td>
                  <td className="muted">{b.template}{b.snapshot_tag && <> <span className="sub inline" title={`forked from ${b.snapshot_tag}`}>· from {b.snapshot_tag}</span></>}</td>
                  <td>{b.node_id}</td>
                  <td><TierPill t={b.tier} />{b.last_error && <span className="err1" title={`${b.last_error}${b.error_attempts ? ` (${b.error_attempts} tries)` : ""}`}>{b.error_attempts ? `${b.error_attempts} tries · ` : ""}{b.last_error}</span>}</td>
                  <td>{b.gateway_port ? <span className="id" title={`${b.ip || ""}:${b.port || ""} inside`}>:{b.gateway_port}</span> : <span className="sub">node-local</span>}</td>
                  <td><button type="button" className="linkish" title="Change what it may reach"
                    onClick={() => setNetwork({ title: "Network", intro: `What ${b.id} may reach, from as soon as its node has finished what it is doing.`, path: `/v1/sandboxes/${encodeURIComponent(b.id)}/egress`, egress: b.egress })}>{reach(b.egress)}</button></td>
                  <td className="r">{ago(Math.max(0, nowS - b.last_active_at))}</td>
                  <td className="r"><button type="button" className="kebab" aria-haspopup="menu" aria-expanded={menu?.id === b.id ? true : undefined} aria-label={`Actions for ${b.id}`} onClick={(e) => openMenu(e, b.id)}>⋯</button></td>
                </tr>
              )) : <tr><td colSpan={9}><div className="empty">{page.total === 0 && !q && !tier && !node ? "No sandboxes yet. Use “+ New sandbox”, or POST /v1/sandboxes." : "Nothing matches."}</div></td></tr>}
            </tbody>
          </table>
        )}
      </div>
      <div className="menu" id="sbMenu" role="menu" hidden={!menuRow} ref={menuEl}>
        {menuRow && sbActions(menuRow).map(([act, label]) => (
          <Fragment key={act}>
            {act === "delete" && <hr />}
            <button type="button" role="menuitem" className={act === "delete" ? "danger" + (isArmed("menu") ? " armed" : "") : undefined} onClick={(e) => void menuAct(e, act)}>
              {act === "delete" && isArmed("menu") ? "Click again to delete" : label}
            </button>
          </Fragment>
        ))}
      </div>
      <Pager offset={offset} size={size} total={page.total} shown={rows.length}
        go={(o, s) => { if (s !== size) { setSize(s); keepPageSize("sandboxes", s); } setOffset(o); }} />
      <NewSandbox open={newOpen} seq={newSeq} internet={!!byDefault?.internet} onClose={() => setNewOpen(false)} onCreated={() => void load()} />
      <Network target={network} onClose={() => setNetwork(null)} onSaved={(e) => { if (network?.path === "/v1/egress") setDefault(e); flash("Saved"); void load(); }} />
    </section>
  );
}
