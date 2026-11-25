import { useMemo, useState } from "react";
import { ModelWeightEstimate, TokenMix } from "../types";

export interface ModelTableRow {
  tier: string;
  windowType: string;
  mix: TokenMix;
  pctPerUsd: number;
  bucketApiEquivUsd: number;
  model: ModelWeightEstimate;
}

interface Props {
  rows: ModelTableRow[];
}

type SortKey = "usd" | "in" | "out" | "cached" | "model";

function formatTokens(n: number): string {
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}k`;
  return String(n);
}

export function ModelTable({ rows }: Props) {
  const [query, setQuery] = useState("");
  const [showRetired, setShowRetired] = useState(false);
  const [sortKey, setSortKey] = useState<SortKey>("usd");
  const [sortAsc, setSortAsc] = useState(false);

  const retiredCount = useMemo(
    () => rows.filter((r) => r.model.status === "retired").length,
    [rows]
  );

  const sorted = useMemo(() => {
    const q = query.trim().toLowerCase();
    const filtered = rows.filter((r) => {
      if (!showRetired && r.model.status === "retired") return false;
      if (!q) return true;
      return (
        r.model.model.toLowerCase().includes(q) ||
        r.tier.toLowerCase().includes(q) ||
        r.windowType.toLowerCase().includes(q)
      );
    });

    const dir = sortAsc ? 1 : -1;
    return [...filtered].sort((a, b) => {
      switch (sortKey) {
        case "usd":
          return dir * (a.model.total_api_usd - b.model.total_api_usd);
        case "in":
          return dir * (a.model.pct_per_1m_in - b.model.pct_per_1m_in);
        case "out":
          return dir * (a.model.pct_per_1m_out - b.model.pct_per_1m_out);
        case "cached":
          return dir * (a.model.pct_per_1m_cached_in - b.model.pct_per_1m_cached_in);
        case "model":
          return dir * a.model.model.localeCompare(b.model.model);
      }
    });
  }, [rows, query, showRetired, sortKey, sortAsc]);

  const toggleSort = (k: SortKey) => {
    if (sortKey === k) setSortAsc(!sortAsc);
    else {
      setSortKey(k);
      setSortAsc(k === "model");
    }
  };

  return (
    <div className="panel" style={{ marginTop: 0 }}>
      <div className="panel-head">
        <span className="panel-title">Model Cost & Usage</span>
        <div className="controls">
          <input
            type="text"
            className="input"
            placeholder="Filter model / bucket..."
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            style={{ width: 170 }}
          />
          {retiredCount > 0 && (
            <button
              type="button"
              className={`btn btn-sm ${showRetired ? "active" : ""}`}
              onClick={() => setShowRetired(!showRetired)}
            >
              Retired ({retiredCount})
            </button>
          )}
        </div>
      </div>

      {sorted.length === 0 ? (
        <div className="empty">No model records match the current filter.</div>
      ) : (
        <div className="table-wrap">
          <table className="data-table">
            <thead>
              <tr>
                <th onClick={() => toggleSort("model")} style={{ cursor: "pointer" }}>
                  Model / Bucket
                </th>
                <th onClick={() => toggleSort("in")} style={{ cursor: "pointer" }}>
                  %/1M In
                </th>
                <th onClick={() => toggleSort("out")} style={{ cursor: "pointer" }}>
                  %/1M Out
                </th>
                <th onClick={() => toggleSort("cached")} style={{ cursor: "pointer" }}>
                  %/1M Cached
                </th>
                <th onClick={() => toggleSort("usd")} style={{ cursor: "pointer" }}>
                  Observed Usage
                </th>
              </tr>
            </thead>
            <tbody>
              {sorted.map((r) => {
                const m = r.model;
                const totalTok =
                  m.total_tokens_in + m.total_tokens_out + m.total_tokens_cached_in;
                return (
                  <tr
                    key={`${r.tier}-${r.windowType}-${m.model}`}
                    style={{ opacity: m.status === "retired" ? 0.55 : 1 }}
                  >
                    <td>
                      <div style={{ fontWeight: 600 }}>{m.model}</div>
                      <div className="mono" style={{ fontSize: 11, opacity: 0.7 }}>
                        {r.windowType} ·{" "}
                        {m.priced
                          ? `$${m.usd_per_1m_in}/$${m.usd_per_1m_out}/$${m.usd_per_1m_cached_in}`
                          : "unpriced"}
                      </div>
                    </td>
                    <td className="mono">{m.pct_per_1m_in.toFixed(2)}%</td>
                    <td className="mono">{m.pct_per_1m_out.toFixed(2)}%</td>
                    <td className="mono">{m.pct_per_1m_cached_in.toFixed(3)}%</td>
                    <td className="mono" style={{ fontSize: 11 }}>
                      <div>
                        ${m.total_api_usd.toFixed(2)} ({m.total_requests} reqs)
                      </div>
                      <div style={{ opacity: 0.7 }}>{formatTokens(totalTok)} tok</div>
                    </td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      )}
    </div>
  );
}
