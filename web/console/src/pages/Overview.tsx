// The overview: the fleet now and over the range, for the whole fleet or the
// host group picked in the header.
import { useEffect, useLayoutEffect, useRef } from "react";
import type { CSSProperties, ReactNode } from "react";
import type { ConsoleNow } from "../types";
import { dur, fmt, perSec } from "../format";
import { st } from "../state";
import { floorMib, last, line, recent, scoped, smooth, sumAt } from "../series";
import { TIERS, TIER_LABEL, css, tierColor } from "../theme";
import { Legend, Spark, TimeChart } from "../charts/Charts";
import { useTween } from "../charts/tween";
import { FLOW_MOVES, drawFlow, flowHover, flowLabel, flowRates } from "../charts/flow";
import { drawRoom, roomHover } from "../charts/room";
import { hideTip } from "../charts/tip";
import type { ChartSeries } from "../charts/timeChart";
import type { Point } from "../types";

// An API p99 past this is worth a look: the master's calls are reads and
// writes of its own store, normally a few milliseconds.
const API_P99_WARN_MS = 250;

function Kpi({ label, swatch, value, digits = 0, suffix = "", delta = false, initial = "0", foot, spark, delay, cls = "" }: {
  label: string; swatch?: string; value: number | null | undefined; digits?: number; suffix?: string; delta?: boolean; initial?: string;
  foot: string; spark: ReactNode; delay: string; cls?: string;
}) {
  const ref = useTween<HTMLDivElement>(value, digits, suffix, delta, initial);
  return (
    <section data-tab="overview" className={"card kpi span-2" + cls} style={{ animationDelay: delay }} hidden={st.tab !== "overview"}>
      <div className="label">{swatch && <span className="swatch" style={{ background: swatch }}></span>}{label}</div>
      <div className="value num" ref={ref}></div>
      <div className="foot">{foot}</div>
      {spark}
    </section>
  );
}

function TierFlow({ counts, idle }: { counts: Record<string, number>; idle: [number, number, number] | null }) {
  const box = useRef<HTMLDivElement>(null);
  const rates = st.S ? flowRates() : {};
  const data = useRef({ counts, rates, idle });
  data.current = { counts, rates, idle };
  // The placeholders before the master first answers.
  useLayoutEffect(() => {
    for (const t of TIERS) box.current!.querySelector("#f-cnt-" + t)!.textContent = "0";
    for (const [k] of FLOW_MOVES) box.current!.querySelector("#f-r-" + k)!.textContent = "–";
  }, []);
  const { v } = st.draw, on = st.tab === "overview";
  useLayoutEffect(() => {
    if (on && box.current && st.S && st.NOW) drawFlow(box.current, counts, rates, idle);
    // drawn on the store's redraws, as before
  }, [v]);
  useEffect(() => {
    const b = box.current!;
    const move = (e: MouseEvent) => flowHover(e, data.current.counts, data.current.rates, data.current.idle);
    b.addEventListener("mousemove", move);
    b.addEventListener("mouseleave", hideTip);
    return () => { b.removeEventListener("mousemove", move); b.removeEventListener("mouseleave", hideTip); };
  }, []);
  return (
    <>
      <div className="topo" id="flow" role="group" ref={box}
        aria-label={st.S && st.NOW ? flowLabel(counts, rates) : "Sandboxes by tier, and the rates between tiers"}>
        <svg className="edges" aria-hidden="true"></svg>
        {TIERS.map((t, i) => (
          <div className="tnode" data-t={t} style={{ "--row": i + 1 } as CSSProperties} key={t}>
            <div className="tl">
              <div className="th"><i style={{ background: `var(--${t})` }}></i>{TIER_LABEL[t]}<span className="ti" id={`f-hint-${t}`}></span></div>
              <div className="tc num" id={`f-cnt-${t}`}></div>
              <div className="tb"><span className="track"><i id={`f-bar-${t}`} style={{ background: `var(--${t})` }}></i></span><span className="tp" id={`f-pct-${t}`}></span></div>
            </div>
            <svg className="tsp" id={`f-sp-${t}`} aria-hidden="true"></svg>
          </div>
        ))}
        {FLOW_MOVES.map(([k]) => <div className="elab" data-k={k} id={`f-l-${k}`} key={k}><b id={`f-r-${k}`}></b></div>)}
      </div>
      <div className="flowfoot">
        {st.S && st.NOW && on && <>deleted <b>{perSec(rates.delete || 0)}</b> · from any tier{st.pool ? " · rates are the whole fleet's" : ""}</>}
      </div>
    </>
  );
}

function Room({ now }: { now: ConsoleNow | null }) {
  const box = useRef<HTMLDivElement>(null);
  const { v } = st.draw, on = st.tab === "overview";
  useLayoutEffect(() => {
    if (on && box.current && st.NOW && st.S) drawRoom(box.current, scoped(st.NOW).nodes, floorMib());
    // drawn on the store's redraws, as before
  }, [v]);
  useEffect(() => {
    const b = box.current!;
    const move = (e: MouseEvent) => roomHover(e, st.NOW ? st.NOW.nodes : []);
    b.addEventListener("mousemove", move);
    b.addEventListener("mouseleave", hideTip);
    return () => { b.removeEventListener("mousemove", move); b.removeEventListener("mouseleave", hideTip); };
  }, []);
  const floor = floorMib(), nodes = now ? scoped(now).nodes : [];
  const low = nodes.filter((n) => n.connected && n.free_mib < floor).length;
  return (
    <>
      <h2>Node headroom</h2><div className="sub">{now && nodes.length ? (low ? `${low} under the floor` : "Tightest first") + (floor > 0 ? ` · line: ${fmt(floor)} MiB floor` : "") : "Free memory, tightest first"}</div>
      <div className="room" id="room" ref={box}></div>
    </>
  );
}

export function Overview() {
  const now = st.NOW, ready = !!(st.NOW && st.S), hidden = st.tab !== "overview";
  const fleetwide = !st.pool;
  // The overview's figures for now: the KPIs and the tier flow, for the
  // whole fleet or the picked host group.
  const sc = now ? scoped(now) : null, what = st.pool ? "the group" : "the fleet";
  const tiers: Record<string, number> = Object.fromEntries(TIERS.map((t) => [t, sc ? sc.tiers[t] || 0 : 0]));
  const total = TIERS.reduce((a, t) => a + tiers[t], 0);
  const up = sc ? sc.nodes.filter((n) => n.connected).length : 0;
  // Refusals over the last half-minute, as the sparkline's colour reads
  // them, so the line and the words never disagree.
  const cr = ready ? last(line("rate.creates", "placed")) : null, rf = ready ? recent(line("rate.creates", "refused")) : 0;
  const p99 = ready ? last(line("latency.api", "p99")) : null;

  // The charts. Rates are smoothed only on the 5 s step of the shortest range.
  const sm = st.S && st.S.step <= 5 ? smooth : (p: Point[]) => p;
  const tiersSeries: ChartSeries[] = TIERS.map((t) => ({ label: TIER_LABEL[t], color: tierColor(t), points: line("fleet.sandboxes", t) }));
  const rates: ChartSeries[] = [
    { label: "created", color: tierColor("hot"), points: sm(line("rate.creates", "placed")) },
    { label: "to disk", color: tierColor("snapshot"), points: sm(line("rate.demotions", "snapshot")) },
    { label: "woken", color: tierColor("frozen"), points: sm(line("rate.wakes")) },
    { label: "refused", color: css("--bad"), points: sm(line("rate.creates", "refused")), dash: "4 3" },
  ];
  const lat: ChartSeries[] = [
    { label: "API p99", color: tierColor("hot"), points: sm(line("latency.api", "p99")) },
    { label: "API p50", color: tierColor("frozen"), points: sm(line("latency.api", "p50")) },
    { label: "create p99", color: tierColor("snapshot"), points: sm(line("latency.create", "p99")) },
    { label: "store wait p99", color: tierColor("detached"), points: sm(line("latency.store_wait", "p99")), dash: "4 3", width: 1.5 },
  ];
  // KPI sparklines: a quiet grey, but for Awake, which is its tier's line,
  // and for a figure that has gone wrong, which takes the colour that says so.
  const quiet = css("--spark");
  const totalPts = sumAt("fleet.sandboxes", TIERS);
  const hotPts = line("fleet.sandboxes", "hot"), hotMap = new Map(hotPts);
  const silent = now ? now.nodes.some((n) => !n.connected) : false;
  const refusing = recent(line("rate.creates", "refused")) > 0;
  const slow = (last(line("latency.api", "p99")) || 0) > API_P99_WARN_MS;

  return (
    <>
      <Kpi cls=" first" delay=".02s" label="Sandboxes" value={ready ? total : undefined} delta
        foot={now ? `${fmt(now.snapshots)} snapshot tags · ${fmt(now.snapshot_bytes / 2 ** 30, 1)} GiB` : ""}
        spark={<Spark points={totalPts} color={quiet} tip={{ label: "sandboxes", f: (v) => fmt(v) }} />} />
      <Kpi delay=".06s" label="Awake" swatch="var(--hot)" value={ready ? tiers.hot : undefined} delta
        foot={ready && total ? `${fmt(100 * tiers.hot / total, 0)}% of ${what}` : ""}
        spark={<Spark points={hotPts} color={tierColor("hot")} tip={{ label: "awake", f: (v) => fmt(v) }} />} />
      <Kpi delay=".10s" label="Sandboxes per awake one" value={ready ? total / Math.max(1, tiers.hot) : undefined} digits={1} suffix="<small>×</small>"
        foot="how far idle tiers stretch memory"
        spark={<Spark points={totalPts.map(([t, v]) => [t, v / Math.max(1, hotMap.get(t) || 1)])} color={quiet} tip={{ label: "per awake one", f: (v) => fmt(v, 1) + "×" }} />} />
      <Kpi delay=".14s" label="Nodes" value={ready ? up : undefined} suffix={`<small>/ ${sc ? sc.nodes.length : 0}</small>`} delta
        foot={sc ? (up === sc.nodes.length ? "all answering" : `${sc.nodes.length - up} silent`) : ""}
        spark={<Spark points={line("fleet.nodes", "up")} color={silent ? css("--bad") : quiet} tip={{ label: "answering", f: (v) => fmt(v) }} />} />
      <Kpi delay=".18s" label="Creates / min" value={ready ? (cr || 0) * 60 : undefined}
        foot={ready ? (rf > 0 ? `${fmt(rf * 60, rf * 60 < 10 ? 1 : 0)} refused / min` : "none refused") + (st.pool ? " · whole fleet" : "") : ""}
        spark={<Spark points={line("rate.creates", "placed").map(([t, v]) => [t, v * 60])} color={refusing ? css("--warn") : quiet} tip={{ label: "creates / min", f: (v) => fmt(v) }} />} />
      <Kpi cls=" last" delay=".22s" label="API p99" value={ready ? p99 : undefined} initial="–" digits={p99 != null && p99 < 10 ? 1 : 0} suffix="<small>ms</small>"
        foot={now && ready ? `up ${dur(now.uptime_secs)} · ${fmt(last(line("rate.api", "requests")) || 0, 1)} req/s` + (st.pool ? " · whole fleet" : "") : ""}
        spark={<Spark points={line("latency.api", "p99")} color={slow ? css("--warn") : quiet} tip={{ label: "API p99", f: (v) => fmt(v, v < 10 ? 1 : 0) + " ms" }} />} />

      <section data-tab="overview" className="card span-6 flowsec" style={{ animationDelay: ".25s" }} hidden={hidden}>
        <h2>Tier flow</h2>
        <div className="sub" id="flowSub">Where the sandboxes are now, and how fast they have moved between tiers over the last half-minute.</div>
        <TierFlow counts={tiers} idle={now ? now.idle : null} />
      </section>

      <section data-tab="overview" className="card span-3 chartsec" style={{ animationDelay: ".34s" }} hidden={hidden}>
        <h2>Sandboxes by tier</h2><div className="sub" id="tiersSub">Over the selected range</div>
        <Legend items={ready ? TIERS.map((t) => [TIER_LABEL[t], tierColor(t)]) : []} />
        <TimeChart series={tiersSeries} height={250} opts={{ kind: "stack" }} />
      </section>
      <section data-tab="overview" className="card span-3 chartsec" style={{ animationDelay: ".38s" }} hidden={hidden}>
        <h2>Lifecycle rates</h2><div className="sub">Per second<span className="fleetwide" hidden={fleetwide}> · whole fleet</span></div>
        <Legend items={ready ? rates.map((r) => [r.label, r.color, r.dash]) : []} />
        <TimeChart series={rates} height={250} opts={{ unit: "/s" }} />
      </section>
      <section data-tab="overview" className="card span-3 chartsec" style={{ animationDelay: ".42s" }} hidden={hidden}>
        <Room now={now} />
      </section>
      <section data-tab="overview" className="card span-3 chartsec" style={{ animationDelay: ".46s" }} hidden={hidden}>
        <h2>Latency</h2><div className="sub"><span className="fleetwide" hidden={fleetwide}>Whole fleet. </span>Milliseconds, log scale. The API, creates, and waiting for the store lock.</div>
        <Legend items={ready ? lat.map((r) => [r.label, r.color, r.dash]) : []} />
        <TimeChart series={lat} height={210} opts={{ log: true, unit: " ms" }} />
      </section>
    </>
  );
}
