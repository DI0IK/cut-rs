import { useMemo, useState } from "react";
import { HistorySnapshotPoint } from "../types";

export type CostMetric = "equiv" | "rate";

interface Props {
  points: HistorySnapshotPoint[];
  metric: CostMetric;
  onMetricChange: (m: CostMetric) => void;
}

const PALETTE = [
  "#1d4ed8",
  "#15803d",
  "#c2410c",
  "#7e22ce",
  "#0f766e",
  "#be123c",
];

function formatAxisTime(ts: number, spanSecs: number): string {
  const d = new Date(ts * 1000);
  if (spanSecs <= 48 * 3600) {
    return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  }
  return d.toLocaleDateString([], { month: "short", day: "numeric" });
}

export function TimelineChart({ points, metric, onMetricChange }: Props) {
  const [hidden, setHidden] = useState<Record<string, boolean>>({});
  const [hoverIdx, setHoverIdx] = useState<number | null>(null);

  const { allSeries, timestamps } = useMemo(() => {
    const byKey = new Map<
      string,
      { ts: number; equiv: number; rate: number; samples: number }[]
    >();
    const tsSet = new Set<number>();

    for (const p of points) {
      if (
        !Number.isFinite(p.bucket_api_equivalent_usd) ||
        p.bucket_api_equivalent_usd <= 0
      ) {
        continue;
      }
      const k = `${p.tier} · ${p.window_type}`;
      tsSet.add(p.computed_at);
      let list = byKey.get(k);
      if (!list) {
        list = [];
        byKey.set(k, list);
      }
      list.push({
        ts: p.computed_at,
        equiv: p.bucket_api_equivalent_usd,
        rate: p.pct_per_usd,
        samples: p.sample_count_window,
      });
    }

    const sortedTs = Array.from(tsSet).sort((a, b) => a - b);
    const list = Array.from(byKey.entries())
      .map(([label, pts], idx) => {
        pts.sort((a, b) => a.ts - b.ts);
        return {
          label,
          color: PALETTE[idx % PALETTE.length],
          pts,
          latest: pts[pts.length - 1],
        };
      })
      .sort((a, b) => a.label.localeCompare(b.label));

    return { allSeries: list, timestamps: sortedTs };
  }, [points]);

  const visibleSeries = allSeries.filter((s) => !hidden[s.label]);

  const { minTs, maxTs, minVal, maxVal } = useMemo(() => {
    let mnT = Infinity;
    let mxT = -Infinity;
    let mnV = Infinity;
    let mxV = -Infinity;
    for (const s of visibleSeries) {
      for (const p of s.pts) {
        const val = metric === "equiv" ? p.equiv : p.rate;
        if (p.ts < mnT) mnT = p.ts;
        if (p.ts > mxT) mxT = p.ts;
        if (val < mnV) mnV = val;
        if (val > mxV) mxV = val;
      }
    }
    if (!Number.isFinite(mnT)) {
      return { minTs: 0, maxTs: 1, minVal: 0, maxVal: 1 };
    }
    const pad = Math.max(metric === "equiv" ? 5 : 0.25, (mxV - mnV) * 0.15);
    return {
      minTs: mnT,
      maxTs: Math.max(mnT + 1, mxT),
      minVal: Math.max(0, mnV - pad),
      maxVal: mxV + pad,
    };
  }, [visibleSeries, metric]);

  const width = 920;
  const height = 220;
  const pad = { top: 14, right: 20, bottom: 26, left: 56 };
  const plotW = width - pad.left - pad.right;
  const plotH = height - pad.top - pad.bottom;
  const span = Math.max(1, maxTs - minTs);

  const xOf = (ts: number) =>
    timestamps.length <= 1
      ? pad.left + plotW / 2
      : pad.left + ((ts - minTs) / span) * plotW;
  const yOf = (v: number) =>
    pad.top + plotH - ((v - minVal) / Math.max(1e-6, maxVal - minVal)) * plotH;

  const hoverTs = hoverIdx !== null ? timestamps[hoverIdx] : null;

  const toggleSeries = (label: string) => {
    setHidden((prev) => ({ ...prev, [label]: !prev[label] }));
  };

  const nearestPoint = (
    pts: { ts: number; equiv: number; rate: number; samples: number }[],
    ts: number
  ) => {
    let best = pts[0];
    let bestD = Infinity;
    for (const pt of pts) {
      const d = Math.abs(pt.ts - ts);
      if (d < bestD) {
        bestD = d;
        best = pt;
      }
    }
    return best;
  };

  return (
    <section className="panel">
      <div className="panel-head">
        <span className="panel-title">
          {metric === "equiv"
            ? "Bucket API Equivalent ($) Over Time"
            : "Quota Burn Rate (%/USD) Over Time"}
        </span>
        <div className="controls">
          <button
            type="button"
            className={`btn btn-sm ${metric === "equiv" ? "active" : ""}`}
            onClick={() => onMetricChange("equiv")}
          >
            Bucket $ Equiv
          </button>
          <button
            type="button"
            className={`btn btn-sm ${metric === "rate" ? "active" : ""}`}
            onClick={() => onMetricChange("rate")}
          >
            % / USD
          </button>
        </div>
      </div>

      <div className="panel-body">
        {allSeries.length === 0 ? (
          <div className="empty-state">
            No calibrated history points yet (requires at least 10 intervals).
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
                const targetTs = minTs + ratio * span;
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
              {[0, 0.25, 0.5, 0.75, 1].map((t) => {
                const v = minVal + t * (maxVal - minVal);
                const y = yOf(v);
                return (
                  <g key={t}>
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
                      fontSize={11}
                      fill="#737168"
                    >
                      {metric === "equiv" ? `$${v.toFixed(0)}` : `${v.toFixed(1)}%`}
                    </text>
                  </g>
                );
              })}

              {timestamps.length > 1 &&
                [0, 0.25, 0.5, 0.75, 1].map((frac) => {
                  const ts = Math.round(minTs + frac * span);
                  return (
                    <text
                      key={frac}
                      x={xOf(ts)}
                      y={height - 6}
                      textAnchor="middle"
                      fontSize={10}
                      fill="#737168"
                    >
                      {formatAxisTime(ts, span)}
                    </text>
                  );
                })}

              {visibleSeries.map((s) => {
                const d = s.pts
                  .map(
                    (p, i) =>
                      `${i === 0 ? "M" : "L"} ${xOf(p.ts).toFixed(1)} ${yOf(
                        metric === "equiv" ? p.equiv : p.rate
                      ).toFixed(1)}`
                  )
                  .join(" ");
                return (
                  <g key={s.label}>
                    {s.pts.length > 1 && (
                      <path
                        d={d}
                        fill="none"
                        stroke={s.color}
                        strokeWidth={2}
                        strokeLinejoin="round"
                      />
                    )}
                    {s.pts.length === 1 && (
                      <circle
                        cx={xOf(s.pts[0].ts)}
                        cy={yOf(metric === "equiv" ? s.pts[0].equiv : s.pts[0].rate)}
                        r={4}
                        fill={s.color}
                      />
                    )}
                  </g>
                );
              })}

              {hoverTs !== null && (
                <g>
                  <line
                    x1={xOf(hoverTs)}
                    x2={xOf(hoverTs)}
                    y1={pad.top}
                    y2={height - pad.bottom}
                    stroke="#9a978c"
                    strokeDasharray="3 3"
                    strokeWidth={1}
                  />
                  {visibleSeries.map((s) => {
                    const nearest = nearestPoint(s.pts, hoverTs);
                    if (!nearest) return null;
                    const v = metric === "equiv" ? nearest.equiv : nearest.rate;
                    return (
                      <circle
                        key={s.label}
                        cx={xOf(nearest.ts)}
                        cy={yOf(v)}
                        r={4.5}
                        fill={s.color}
                        stroke="#ffffff"
                        strokeWidth={1.5}
                      />
                    );
                  })}
                </g>
              )}
            </svg>

            <div className="legend-wrap" style={{ marginTop: 10 }}>
              <div className="legend">
                {allSeries.map((s) => {
                  const isOff = !!hidden[s.label];
                  const pt = hoverTs !== null ? nearestPoint(s.pts, hoverTs) : s.latest;
                  const val = pt
                    ? metric === "equiv"
                      ? `$${pt.equiv.toFixed(2)}`
                      : `${pt.rate.toFixed(2)}%/USD`
                    : "--";
                  return (
                    <button
                      key={s.label}
                      type="button"
                      className={`legend-item legend-btn ${isOff ? "legend-off" : ""}`}
                      onClick={() => toggleSeries(s.label)}
                    >
                      <span
                        className="legend-swatch"
                        style={{ backgroundColor: s.color }}
                      />
                      <span>{s.label}</span>
                      <span className="mono" style={{ fontWeight: 600 }}>
                        {val}
                      </span>
                    </button>
                  );
                })}
              </div>
              <span className="mono" style={{ fontSize: 11, color: "var(--ink-muted)" }}>
                {hoverTs !== null
                  ? new Date(hoverTs * 1000).toLocaleString()
                  : "Latest"}
              </span>
            </div>
          </>
        )}
      </div>
    </section>
  );
}
