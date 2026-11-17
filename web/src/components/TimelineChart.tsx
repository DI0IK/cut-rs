import { useMemo, useState } from "react";
import { HistorySnapshotPoint } from "../types";

export type CostMetric = "blended" | "out" | "in" | "cached";

interface Props {
  points: HistorySnapshotPoint[];
  metric: CostMetric;
  onMetricChange: (m: CostMetric) => void;
}

const PALETTE = [
  "#c2410c",
  "#1d4ed8",
  "#15803d",
  "#7e22ce",
  "#b45309",
  "#0f766e",
  "#be123c",
];

function metricValue(
  p: HistorySnapshotPoint,
  m: HistorySnapshotPoint["models"][number],
  metric: CostMetric
): number {
  switch (metric) {
    case "in":
      return m.pct_per_1m_in;
    case "out":
      return m.pct_per_1m_out;
    case "cached":
      return m.pct_per_1m_cached_in;
    case "blended": {
      const mix = p.token_mix;
      return (
        mix.in_share * m.pct_per_1m_in +
        mix.out_share * m.pct_per_1m_out +
        mix.cached_share * m.pct_per_1m_cached_in
      );
    }
  }
}

export function TimelineChart({ points, metric, onMetricChange }: Props) {
  const [hoverIdx, setHoverIdx] = useState<number | null>(null);

  const { series, timestamps, maxVal, minTs, maxTs, regimeMarkers } = useMemo(() => {
    const bySeries = new Map<
      string,
      { label: string; pts: { ts: number; val: number }[] }
    >();
    const tsSet = new Set<number>();
    const markers = new Set<number>();

    for (const pt of points) {
      tsSet.add(pt.computed_at);
      for (const ev of pt.recent_change_events) {
        if (ev.event_type === "regime_shift") {
          markers.add(ev.timestamp);
        }
      }
      for (const m of pt.models) {
        const key = `${pt.tier} · ${pt.window_type} · ${m.model}`;
        let entry = bySeries.get(key);
        if (!entry) {
          entry = { label: key, pts: [] };
          bySeries.set(key, entry);
        }
        entry.pts.push({ ts: pt.computed_at, val: metricValue(pt, m, metric) });
      }
    }

    const sortedTs = Array.from(tsSet).sort((a, b) => a - b);
    const seriesArr = Array.from(bySeries.values()).map((s) => ({
      ...s,
      pts: s.pts.sort((a, b) => a.ts - b.ts),
    }));

    let mx = 0.1;
    for (const s of seriesArr) {
      for (const p of s.pts) {
        if (p.val > mx) mx = p.val;
      }
    }

    const minT = sortedTs[0] ?? 0;
    const maxT = sortedTs[sortedTs.length - 1] ?? minT + 1;
    return {
      series: seriesArr,
      timestamps: sortedTs,
      maxVal: mx * 1.12,
      minTs: minT,
      maxTs: maxT === minT ? minT + 1 : maxT,
      regimeMarkers: Array.from(markers).filter((t) => t >= minT && t <= maxT),
    };
  }, [points, metric]);

  const width = 920;
  const height = 250;
  const pad = { top: 14, right: 16, bottom: 28, left: 52 };
  const plotW = width - pad.left - pad.right;
  const plotH = height - pad.top - pad.bottom;

  const xOf = (ts: number) => pad.left + ((ts - minTs) / (maxTs - minTs)) * plotW;
  const yOf = (val: number) => pad.top + plotH - (val / maxVal) * plotH;

  const hoverTs = hoverIdx !== null ? timestamps[hoverIdx] : null;

  return (
    <div className="panel">
      <div className="panel-head">
        <span className="panel-title">
          Estimated Quota Cost Timeline (% per 1M tokens)
        </span>
        <div className="controls">
          {(["blended", "out", "in", "cached"] as CostMetric[]).map((m) => (
            <button
              key={m}
              type="button"
              className={`btn ${metric === m ? "active" : ""}`}
              onClick={() => onMetricChange(m)}
            >
              {m === "blended" ? "Blended Mix" : m.toUpperCase()}
            </button>
          ))}
        </div>
      </div>

      <div className="panel-body">
        {series.length === 0 ? (
          <div className="empty-state">
            No snapshot history recorded for this selection yet.
          </div>
        ) : (
          <>
            <svg
              viewBox={`0 0 ${width} ${height}`}
              style={{ width: "100%", height: "auto", display: "block" }}
              onMouseLeave={() => setHoverIdx(null)}
              onMouseMove={(e) => {
                if (timestamps.length === 0) return;
                const rect = e.currentTarget.getBoundingClientRect();
                const relX = ((e.clientX - rect.left) / rect.width) * width;
                const ratio = Math.max(0, Math.min(1, (relX - pad.left) / plotW));
                const targetTs = minTs + ratio * (maxTs - minTs);
                let best = 0;
                let bestDist = Infinity;
                timestamps.forEach((t, idx) => {
                  const d = Math.abs(t - targetTs);
                  if (d < bestDist) {
                    bestDist = d;
                    best = idx;
                  }
                });
                setHoverIdx(best);
              }}
            >
              {[0, 0.25, 0.5, 0.75, 1].map((frac) => {
                const v = maxVal * frac;
                const y = yOf(v);
                return (
                  <g key={frac}>
                    <line
                      x1={pad.left}
                      x2={width - pad.right}
                      y1={y}
                      y2={y}
                      stroke="#eceae2"
                      strokeWidth={1}
                    />
                    <text
                      x={pad.left - 8}
                      y={y + 4}
                      textAnchor="end"
                      fontSize={10}
                      fill="#71717a"
                      fontFamily="ui-monospace, monospace"
                    >
                      {v.toFixed(1)}%
                    </text>
                  </g>
                );
              })}

              {regimeMarkers.map((t) => (
                <line
                  key={t}
                  x1={xOf(t)}
                  x2={xOf(t)}
                  y1={pad.top}
                  y2={pad.top + plotH}
                  stroke="#b91c1c"
                  strokeDasharray="4 3"
                  strokeWidth={1}
                />
              ))}

              {series.map((s, idx) => {
                const color = PALETTE[idx % PALETTE.length];
                const d = s.pts
                  .map((p, i) => `${i === 0 ? "M" : "L"}${xOf(p.ts).toFixed(1)},${yOf(p.val).toFixed(1)}`)
                  .join(" ");
                return (
                  <g key={s.label}>
                    <path d={d} fill="none" stroke={color} strokeWidth={1.75} />
                    {s.pts.map((p, i) => (
                      <circle
                        key={`${s.label}-${i}`}
                        cx={xOf(p.ts)}
                        cy={yOf(p.val)}
                        r={s.pts.length === 1 ? 3.5 : 2}
                        fill={color}
                      />
                    ))}
                  </g>
                );
              })}

              {hoverTs !== null && (
                <line
                  x1={xOf(hoverTs)}
                  x2={xOf(hoverTs)}
                  y1={pad.top}
                  y2={pad.top + plotH}
                  stroke="#18181b"
                  strokeWidth={1}
                />
              )}

              <text
                x={pad.left}
                y={height - 6}
                fontSize={10}
                fill="#71717a"
                fontFamily="ui-monospace, monospace"
              >
                {new Date(minTs * 1000).toLocaleString()}
              </text>
              <text
                x={width - pad.right}
                y={height - 6}
                textAnchor="end"
                fontSize={10}
                fill="#71717a"
                fontFamily="ui-monospace, monospace"
              >
                {new Date(maxTs * 1000).toLocaleString()}
              </text>
            </svg>

            <div className="legend">
              {series.map((s, idx) => {
                const color = PALETTE[idx % PALETTE.length];
                const pointAtHover =
                  hoverTs !== null
                    ? s.pts.find((p) => p.ts === hoverTs) ?? s.pts[s.pts.length - 1]
                    : s.pts[s.pts.length - 1];
                return (
                  <span key={s.label} className="legend-item">
                    <span className="legend-swatch" style={{ background: color }} />
                    <span>{s.label}:</span>
                    <strong>{pointAtHover ? `${pointAtHover.val.toFixed(2)}%` : "--"}</strong>
                  </span>
                );
              })}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
