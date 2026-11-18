import { useMemo, useState } from "react";
import { QuotaSnapshotRow } from "../types";

interface Props {
  history: QuotaSnapshotRow[];
}

type RangeFilter = "24h" | "7d" | "all";

const PALETTE = [
  "#1d4ed8",
  "#15803d",
  "#c2410c",
  "#7e22ce",
  "#0f766e",
  "#be123c",
  "#b45309",
  "#4338ca",
];

function formatAxisTime(ts: number, spanSecs: number): string {
  const d = new Date(ts * 1000);
  if (spanSecs <= 48 * 3600) {
    return d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  }
  return d.toLocaleDateString([], { month: "short", day: "numeric" });
}

export function QuotaBurndownChart({ history }: Props) {
  const [range, setRange] = useState<RangeFilter>("7d");
  const [hiddenBuckets, setHiddenBuckets] = useState<Record<string, boolean>>({});
  const [hoverIdx, setHoverIdx] = useState<number | null>(null);

  const filtered = useMemo(() => {
    if (range === "all") return history;
    const nowSec = Math.floor(Date.now() / 1000);
    const cutoff = nowSec - (range === "24h" ? 86400 : 7 * 86400);
    return history.filter((r) => r.recorded_at >= cutoff);
  }, [history, range]);

  const { buckets, timestamps, minTs, maxTs } = useMemo(() => {
    const byBucket = new Map<string, { ts: number; val: number }[]>();
    const tsSet = new Set<number>();

    for (const row of filtered) {
      tsSet.add(row.recorded_at);
      let pts = byBucket.get(row.bucket_id);
      if (!pts) {
        pts = [];
        byBucket.set(row.bucket_id, pts);
      }
      pts.push({ ts: row.recorded_at, val: row.remaining_pct });
    }

    const sortedTs = Array.from(tsSet).sort((a, b) => a - b);
    const bucketList = Array.from(byBucket.entries())
      .map(([bucketId, pts], idx) => {
        pts.sort((a, b) => a.ts - b.ts);
        return {
          bucketId,
          pts,
          color: PALETTE[idx % PALETTE.length],
        };
      })
      .sort((a, b) => a.bucketId.localeCompare(b.bucketId));

    return {
      buckets: bucketList,
      timestamps: sortedTs,
      minTs: sortedTs[0] ?? 0,
      maxTs: sortedTs[sortedTs.length - 1] ?? 1,
    };
  }, [filtered]);

  const visibleBuckets = buckets.filter((b) => !hiddenBuckets[b.bucketId]);

  const width = 920;
  const height = 220;
  const pad = { top: 14, right: 20, bottom: 26, left: 46 };
  const plotW = width - pad.left - pad.right;
  const plotH = height - pad.top - pad.bottom;

  const span = Math.max(1, maxTs - minTs);
  const xOf = (ts: number) =>
    timestamps.length <= 1
      ? pad.left + plotW / 2
      : pad.left + ((ts - minTs) / span) * plotW;
  const yOf = (pct: number) =>
    pad.top + plotH - (Math.max(0, Math.min(100, pct)) / 100) * plotH;

  const hoverTs = hoverIdx !== null ? timestamps[hoverIdx] : null;

  const toggleBucket = (bucketId: string) => {
    setHiddenBuckets((prev) => ({ ...prev, [bucketId]: !prev[bucketId] }));
  };

  return (
    <div className="panel">
      <div className="panel-head">
        <span className="panel-title">Quota Burn-Down History (% remaining)</span>
        <div className="controls">
          {(["24h", "7d", "all"] as RangeFilter[]).map((r) => (
            <button
              key={r}
              type="button"
              className={`btn btn-sm ${range === r ? "active" : ""}`}
              onClick={() => setRange(r)}
            >
              {r === "all" ? "All" : r.toUpperCase()}
            </button>
          ))}
        </div>
      </div>

      <div className="panel-body">
        {buckets.length === 0 ? (
          <div className="empty-state">
            No quota snapshot history recorded in this window yet.
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
              {[0, 25, 50, 75, 100].map((pct) => {
                const y = yOf(pct);
                return (
                  <g key={pct}>
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
                      {pct}%
                    </text>
                  </g>
                );
              })}

              {timestamps.length > 1 &&
                [0, 0.25, 0.5, 0.75, 1].map((frac) => {
                  const ts = Math.round(minTs + frac * span);
                  const x = xOf(ts);
                  return (
                    <text
                      key={frac}
                      x={x}
                      y={height - 6}
                      textAnchor="middle"
                      fontSize={10}
                      fill="#737168"
                    >
                      {formatAxisTime(ts, span)}
                    </text>
                  );
                })}

              {visibleBuckets.map((b) => {
                const pathD = b.pts
                  .map(
                    (pt, idx) =>
                      `${idx === 0 ? "M" : "L"} ${xOf(pt.ts).toFixed(1)} ${yOf(pt.val).toFixed(1)}`
                  )
                  .join(" ");
                return (
                  <g key={b.bucketId}>
                    {b.pts.length > 1 && (
                      <path
                        d={pathD}
                        fill="none"
                        stroke={b.color}
                        strokeWidth={2}
                        strokeLinejoin="round"
                      />
                    )}
                    {b.pts.map((pt, idx) => (
                      <circle
                        key={idx}
                        cx={xOf(pt.ts)}
                        cy={yOf(pt.val)}
                        r={b.pts.length === 1 ? 4 : 2.5}
                        fill={b.color}
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
                  y2={height - pad.bottom}
                  stroke="#9a978c"
                  strokeDasharray="3 3"
                  strokeWidth={1}
                />
              )}
            </svg>

            <div
              style={{
                display: "flex",
                flexWrap: "wrap",
                alignItems: "center",
                justifyContent: "space-between",
                gap: 8,
                marginTop: 8,
                fontSize: 12,
              }}
            >
              <div style={{ display: "flex", flexWrap: "wrap", gap: 10 }}>
                {buckets.map((b) => {
                  const hidden = !!hiddenBuckets[b.bucketId];
                  const latest = b.pts[b.pts.length - 1];
                  return (
                    <button
                      key={b.bucketId}
                      type="button"
                      onClick={() => toggleBucket(b.bucketId)}
                      className="btn btn-sm"
                      style={{
                        opacity: hidden ? 0.45 : 1,
                        display: "inline-flex",
                        alignItems: "center",
                        gap: 6,
                      }}
                    >
                      <span
                        style={{
                          width: 8,
                          height: 8,
                          borderRadius: "50%",
                          background: b.color,
                          display: "inline-block",
                        }}
                      />
                      <span>{b.bucketId}</span>
                      {latest && (
                        <span style={{ opacity: 0.7 }}>
                          {latest.val.toFixed(1)}%
                        </span>
                      )}
                    </button>
                  );
                })}
              </div>

              {hoverTs !== null && (
                <div style={{ opacity: 0.8 }}>
                  <strong>{new Date(hoverTs * 1000).toLocaleString()}</strong>
                  {visibleBuckets.map((b) => {
                    let match: { ts: number; val: number } | undefined;
                    for (const pt of b.pts) {
                      if (pt.ts <= hoverTs) match = pt;
                    }
                    if (!match) return null;
                    return (
                      <span key={b.bucketId} style={{ marginLeft: 10 }}>
                        {b.bucketId}: {match.val.toFixed(1)}%
                      </span>
                    );
                  })}
                </div>
              )}
            </div>
          </>
        )}
      </div>
    </div>
  );
}
