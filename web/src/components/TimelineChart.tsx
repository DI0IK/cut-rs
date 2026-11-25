import { useMemo } from "react";
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

export function TimelineChart({ points, metric, onMetricChange }: Props) {
  const series = useMemo(() => {
    const byKey = new Map<string, { ts: number; val: number }[]>();
    for (const p of points) {
      const k = `${p.tier} · ${p.window_type}`;
      const val =
        metric === "equiv" ? p.bucket_api_equivalent_usd : p.pct_per_usd;
      if (!Number.isFinite(val) || val <= 0) continue;
      let list = byKey.get(k);
      if (!list) {
        list = [];
        byKey.set(k, list);
      }
      list.push({ ts: p.computed_at, val });
    }
    return Array.from(byKey.entries()).map(([label, pts], idx) => {
      pts.sort((a, b) => a.ts - b.ts);
      return {
        label,
        color: PALETTE[idx % PALETTE.length],
        pts,
        latest: pts[pts.length - 1]?.val ?? 0,
      };
    });
  }, [points, metric]);

  const { minTs, maxTs, minVal, maxVal } = useMemo(() => {
    let mnT = Infinity;
    let mxT = -Infinity;
    let mnV = Infinity;
    let mxV = -Infinity;
    for (const s of series) {
      for (const p of s.pts) {
        if (p.ts < mnT) mnT = p.ts;
        if (p.ts > mxT) mxT = p.ts;
        if (p.val < mnV) mnV = p.val;
        if (p.val > mxV) mxV = p.val;
      }
    }
    if (!Number.isFinite(mnT)) return { minTs: 0, maxTs: 1, minVal: 0, maxVal: 1 };
    const pad = Math.max(0.5, (mxV - mnV) * 0.15);
    return {
      minTs: mnT,
      maxTs: Math.max(mnT + 1, mxT),
      minVal: Math.max(0, mnV - pad),
      maxVal: mxV + pad,
    };
  }, [series]);

  const W = 760;
  const H = 200;
  const PAD_L = 56;
  const PAD_R = 16;
  const PAD_T = 16;
  const PAD_B = 28;
  const plotW = W - PAD_L - PAD_R;
  const plotH = H - PAD_T - PAD_B;

  const xOf = (ts: number) => PAD_L + ((ts - minTs) / (maxTs - minTs)) * plotW;
  const yOf = (v: number) =>
    PAD_T + plotH - ((v - minVal) / Math.max(1e-6, maxVal - minVal)) * plotH;

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

      {series.length === 0 ? (
        <div className="empty">No calibrated history points yet.</div>
      ) : (
        <div style={{ padding: "8px 0" }}>
          <div style={{ display: "flex", gap: 14, flexWrap: "wrap", marginBottom: 6, fontSize: 12 }}>
            {series.map((s) => (
              <span key={s.label} className="mono" style={{ display: "inline-flex", alignItems: "center", gap: 6 }}>
                <span style={{ width: 10, height: 3, background: s.color, display: "inline-block" }} />
                {s.label}:{" "}
                <strong>
                  {metric === "equiv"
                    ? `$${s.latest.toFixed(2)}`
                    : `${s.latest.toFixed(2)}%/USD`}
                </strong>
              </span>
            ))}
          </div>
          <svg viewBox={`0 0 ${W} ${H}`} style={{ width: "100%", height: "auto", display: "block" }}>
            {[0, 0.5, 1].map((t) => {
              const v = minVal + t * (maxVal - minVal);
              const y = yOf(v);
              return (
                <g key={t}>
                  <line x1={PAD_L} y1={y} x2={W - PAD_R} y2={y} stroke="rgba(128,128,128,0.2)" strokeDasharray="3 3" />
                  <text x={PAD_L - 6} y={y + 4} textAnchor="end" fontSize={10} fill="currentColor" opacity={0.65}>
                    {metric === "equiv" ? `$${v.toFixed(1)}` : `${v.toFixed(1)}%`}
                  </text>
                </g>
              );
            })}
            {series.map((s) => {
              const d = s.pts
                .map((p, i) => `${i === 0 ? "M" : "L"}${xOf(p.ts).toFixed(1)},${yOf(p.val).toFixed(1)}`)
                .join(" ");
              return (
                <path
                  key={s.label}
                  d={d}
                  fill="none"
                  stroke={s.color}
                  strokeWidth={2}
                />
              );
            })}
            <text x={PAD_L} y={H - 6} fontSize={10} fill="currentColor" opacity={0.65}>
              {new Date(minTs * 1000).toLocaleDateString()}
            </text>
            <text x={W - PAD_R} y={H - 6} textAnchor="end" fontSize={10} fill="currentColor" opacity={0.65}>
              {new Date(maxTs * 1000).toLocaleDateString()}
            </text>
          </svg>
        </div>
      )}
    </section>
  );
}
