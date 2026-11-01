// ---- activity ---------------------------------------------------------------------
// Paged and filtered on the master, like the sandboxes: two days of a busy
// fleet is far more than one page.
import { useEffect, useRef, useState } from "react";
import type { ConsoleEvent } from "../types";
import { apiPage, auth } from "../api";
import { ago, errorText } from "../format";
import { st } from "../state";
import { keepPageSize, pageSize } from "../storage";
import { Dropdown } from "../components/Dropdown";
import { Pager } from "../components/Pager";
import { useDebounced, useTabLoad } from "../components/hooks";

// Line icons in the same hand as the header's, coloured by what happened.
const ico = (d: string) => `<svg viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${d}</svg>`;
const I_POWER = ico('<path d="M8 2.2v5.3"/><path d="M4.7 4.3a5 5 0 1 0 6.6 0"/>');
const ICON: Record<string, [string, string]> = {
  node_down: [I_POWER, "--bad"], node_up: [I_POWER, "--good"],
  failing: [ico('<path d="M8 2.5 14 13H2z"/><path d="M8 6.6v2.9"/><path d="M8 11.5h0"/>'), "--warn"],
  quarantined: [ico('<circle cx="8" cy="8" r="5.8"/><path d="M5.2 8h5.6"/>'), "--bad"],
  refused: [ico('<circle cx="8" cy="8" r="5.8"/><path d="M3.9 12.1l8.2-8.2"/>'), "--warn"],
  woken: [ico('<path d="M2.5 8a5.5 5.5 0 1 0 1.6-3.9"/><path d="M2.5 2.5v3h3"/>'), "--hot"],
  to_disk: [ico('<path d="M8 2.2v7.6"/><path d="M4.6 6.6 8 10l3.4-3.4"/><path d="M3 13.6h10"/>'), "--snapshot"],
};
const I_DOT = ico('<circle cx="8" cy="8" r="2" fill="currentColor" stroke="none"/>');
const EV_LABEL: Record<string, string> = { node_down: "Node silent", node_up: "Node back", failing: "Not starting", quarantined: "Quarantined", refused: "Refused", woken: "Woken", to_disk: "To disk" };
const ACT_KINDS: [string, string][] = [["", "All"], ["node_down,node_up", "Nodes"], ["failing,quarantined", "Not starting"], ["refused", "Refused"], ["woken", "Woken"], ["to_disk", "To disk"]];
const SINCE = [{ value: "3600", label: "Last hour" }, { value: "21600", label: "Last 6 hours" }, { value: "86400", label: "Last 24 hours" }, { value: "172800", label: "Last 2 days" }];
const key = (e: ConsoleEvent) => e.ts + e.kind + e.subject + e.detail;

export function Activity() {
  const [kind, setKind] = useState("");
  const [qIn, setQIn] = useState("");
  const q = useDebounced(qIn.trim(), 250);
  const [since, setSince] = useState(86400);
  const [offset, setOffset] = useState(0);
  const [size, setSize] = useState(() => pageSize("activity", 20));
  const [page, setPage] = useState<{ total: number; rows: { e: ConsoleEvent; fresh: boolean }[] } | null>(null);
  const [error, setError] = useState("");
  const busy = useRef(false), seen = useRef<Set<string> | null>(null);
  const want = useRef({ kind, q, since, offset, size });
  want.current = { kind, q, since, offset, size };

  const load = async (): Promise<void> => {
    if (!auth.token || busy.current) return;
    busy.current = true;
    try {
      const w = want.current;
      const qs = new URLSearchParams({ since: String(w.since), limit: String(w.size), offset: String(w.offset) });
      if (w.kind) qs.set("kind", w.kind);
      if (w.q) qs.set("q", w.q);
      const { total, rows } = await apiPage<ConsoleEvent>("/v1/console/events?" + qs);
      if (w.offset >= total && total > 0) { busy.current = false; setOffset(Math.floor((total - 1) / w.size) * w.size); return; }
      // Only rows that arrive while the first page is open are new.
      const s = seen.current, first = w.offset === 0;
      setPage({ total, rows: rows.map((e) => ({ e, fresh: first && !!s && !s.has(key(e)) })) });
      if (first) seen.current = new Set(rows.map(key));
      setError("");
    } catch (e) {
      if (String((e as Error).message) === "401") return;
      setError(errorText(e));
    } finally { busy.current = false; }
  };
  // Shown, and kept up while its first page is open.
  useTabLoad("activity", (why) => { if (why === "show" || want.current.offset === 0) void load(); });
  const first = useRef(true);
  useEffect(() => {
    if (first.current) { first.current = false; return; }
    void load();
    // loads on what is asked for
  }, [kind, q, since, offset, size]);
  useEffect(() => setOffset(0), [q]);

  const nowS = Date.now() / 1000;
  return (
    <section data-tab="activity" className="card span-12" style={{ animationDelay: ".54s" }} hidden={st.tab !== "activity"}>
      <h2>Activity</h2><div className="sub" id="actSub">What the master recorded as it sampled the fleet: nodes going silent and coming back, refusals, wakes, sandboxes that will not start.</div>
      <div className="bar">
        <input id="actQ" type="search" placeholder="Search by node, sandbox or detail" value={qIn} onChange={(e) => setQIn(e.target.value)} />
        <div className="chips" id="actKinds">
          {ACT_KINDS.map(([k, l]) => {
            const ic = ICON[k.split(",")[0]];
            return (
              <button type="button" key={k} data-k={k} aria-pressed={kind === k} onClick={() => { setKind(k); setOffset(0); }}>
                {ic && <span className="kic" style={{ color: `var(${ic[1]})` }} dangerouslySetInnerHTML={{ __html: ic[0] }}></span>}{l}
              </button>
            );
          })}
        </div>
        <Dropdown id="actSince" ariaLabel="How far back" options={SINCE} value={String(since)} onChange={(v) => { setSince(Number(v)); setOffset(0); }} />
      </div>
      <div className="tbl-wrap">
        {error ? <table className="tbl" id="actTable"><tbody><tr><td><div className="empty">{error}</div></td></tr></tbody></table>
          : page && (
            <table className="tbl" id="actTable">
              <thead><tr><th>When</th><th>What</th><th>Node or sandbox</th><th>Detail</th></tr></thead>
              <tbody>
                {page.rows.length ? page.rows.map(({ e, fresh }, i) => {
                  const [ic, col] = ICON[e.kind] || [I_DOT, "--muted"];
                  return (
                    <tr key={key(e) + "|" + i} className={fresh ? "new" : undefined}>
                      <td className="when" title={new Date(e.ts * 1000).toLocaleString()}>{ago(Math.max(0, nowS - e.ts))}</td>
                      <td><span className="evk"><span className="ic" style={{ background: `color-mix(in srgb, var(${col}) 18%, transparent)`, color: `var(${col})` }} dangerouslySetInnerHTML={{ __html: ic }}></span>{EV_LABEL[e.kind] || e.kind}</span></td>
                      <td className="id">{e.subject}</td>
                      <td><div className="wrap">{e.detail}</div></td>
                    </tr>
                  );
                }) : <tr><td colSpan={4}><div className="empty">{kind || q ? "Nothing matches." : "Nothing in this window. Node changes, refusals, wakes and failures land here."}</div></td></tr>}
              </tbody>
            </table>
          )}
      </div>
      {!error && page && <Pager offset={offset} size={size} total={page.total} shown={page.rows.length}
        go={(o, s) => { if (s !== size) { setSize(s); keepPageSize("activity", s); } setOffset(o); }} />}
    </section>
  );
}
