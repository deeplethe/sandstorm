// The hand-drawn charts as components: each owns its <svg> and draws into it
// whenever the store asks for everything to be drawn again.
import { useLayoutEffect, useRef } from "react";
import type { Point } from "../types";
import { st } from "../state";
import { timeChart, type ChartOpts, type ChartSeries } from "./timeChart";
import { spark, type SparkTip } from "./spark";

export function TimeChart({ series, height, opts }: { series: ChartSeries[]; height: number; opts?: ChartOpts }) {
  const ref = useRef<SVGSVGElement>(null);
  const { v, first } = st.draw;
  useLayoutEffect(() => {
    if (ref.current && st.S && st.NOW) timeChart(ref.current, st.S, series, { ...opts, first });
    // drawn on the store's redraws, as before
  }, [v]);
  return <svg className="chart" ref={ref} height={height}></svg>;
}

export function Spark({ points, color, fill = false, tip = null }: { points: Point[]; color: string; fill?: boolean; tip?: SparkTip | null }) {
  const ref = useRef<SVGSVGElement>(null);
  const { v } = st.draw;
  useLayoutEffect(() => {
    if (ref.current && st.S && st.NOW) spark(ref.current, points, color, fill, tip);
    // drawn on the store's redraws, as before
  }, [v]);
  return <svg className="spark" ref={ref}></svg>;
}

/** [label, colour, dash] for each series. */
export function Legend({ items }: { items: [string, string, string?][] }) {
  return (
    <div className="legend">
      {items.map(([label, color, dash]) => (
        <span key={label}>
          {dash
            ? <svg width="16" height="8"><line x1="0" y1="4" x2="16" y2="4" stroke={color} strokeWidth="2" strokeDasharray="3 3" /></svg>
            : <i className="swatch" style={{ background: color }}></i>}
          {label}
        </span>
      ))}
    </div>
  );
}
