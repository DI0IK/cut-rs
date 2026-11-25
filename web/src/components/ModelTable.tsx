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

interface BucketRate {
  windowType: string;
  pctIn: number;
  pctOut: number;
  pctCached: number;
  apiUsd: number;
  requests: number;
  tokensIn: number;
  tokensOut: number;
  tokensCached: number;
}

interface MergedModelRow {
  tier: string;
  baseModel: string;
  variants: string[];
  priced: boolean;
  usdIn: number;
  usdOut: number;
  usdCached: number;
  buckets: BucketRate[];
  totalApiUsd: number;
  totalRequests: number;
  totalTokens: number;
  maxPctIn: number;
  maxPctOut: number;
  maxPctCached: number;
  status: "active" | "retired";
}

interface Props {
  rows: ModelTableRow[];
}

type SortKey = "usd" | "in" | "out" | "cached" | "model";

const THINKING_SUFFIX_RE =
  /^(.*?)\s*\((minimal|low|medium|high|xhigh|max|none|auto)\)$/i;

function splitThinkingMode(name: string): {
  baseModel: string;
  variant: string | null;
} {
  const m = name.trim().match(THINKING_SUFFIX_RE);
  if (m && m[1].trim().length > 0) {
    return { baseModel: m[1].trim(), variant: m[2].toLowerCase() };
  }
  return { baseModel: name.trim(), variant: null };
}

function shortBucket(wt: string): string {
  if (wt.includes("5h")) return "5h";
  if (wt.includes("weekly") || wt.includes("week")) return "wk";
  return wt;
}

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

  const mergedRows = useMemo<MergedModelRow[]>(() => {
    const modelMap = new Map<
      string,
      {
        tier: string;
        baseModel: string;
        variants: string[];
        priced: boolean;
        usdIn: number;
        usdOut: number;
        usdCached: number;
        status: "active" | "retired";
        byBucket: Map<string, BucketRate>;
      }
    >();

    for (const r of rows) {
      const { baseModel, variant } = splitThinkingMode(r.model.model);
      const key = `${r.tier}::${baseModel}`;
      let entry = modelMap.get(key);
      if (!entry) {
        entry = {
          tier: r.tier,
          baseModel,
          variants: [],
          priced: r.model.priced,
          usdIn: r.model.usd_per_1m_in,
          usdOut: r.model.usd_per_1m_out,
          usdCached: r.model.usd_per_1m_cached_in,
          status: r.model.status,
          byBucket: new Map(),
        };
        modelMap.set(key, entry);
      } else if (r.model.status === "active") {
        entry.status = "active";
      }

      if (variant && !entry.variants.includes(variant)) {
        entry.variants.push(variant);
      }

      const b = entry.byBucket.get(r.windowType);
      if (!b) {
        entry.byBucket.set(r.windowType, {
          windowType: r.windowType,
          pctIn: r.model.pct_per_1m_in,
          pctOut: r.model.pct_per_1m_out,
          pctCached: r.model.pct_per_1m_cached_in,
          apiUsd: r.model.total_api_usd,
          requests: r.model.total_requests,
          tokensIn: r.model.total_tokens_in,
          tokensOut: r.model.total_tokens_out,
          tokensCached: r.model.total_tokens_cached_in,
        });
      } else {
        b.apiUsd += r.model.total_api_usd;
        b.requests += r.model.total_requests;
        b.tokensIn += r.model.total_tokens_in;
        b.tokensOut += r.model.total_tokens_out;
        b.tokensCached += r.model.total_tokens_cached_in;
      }
    }

    return Array.from(modelMap.values()).map((entry) => {
      const buckets = Array.from(entry.byBucket.values()).sort((a, b) =>
        a.windowType.localeCompare(b.windowType)
      );
      // Same turns are billed to both 5h and weekly buckets, so take max across buckets
      const totalApiUsd = Math.max(0, ...buckets.map((b) => b.apiUsd));
      const totalRequests = Math.max(0, ...buckets.map((b) => b.requests));
      const totalTokens = Math.max(
        0,
        ...buckets.map((b) => b.tokensIn + b.tokensOut + b.tokensCached)
      );
      return {
        tier: entry.tier,
        baseModel: entry.baseModel,
        variants: entry.variants,
        priced: entry.priced,
        usdIn: entry.usdIn,
        usdOut: entry.usdOut,
        usdCached: entry.usdCached,
        buckets,
        totalApiUsd,
        totalRequests,
        totalTokens,
        maxPctIn: Math.max(0, ...buckets.map((b) => b.pctIn)),
        maxPctOut: Math.max(0, ...buckets.map((b) => b.pctOut)),
        maxPctCached: Math.max(0, ...buckets.map((b) => b.pctCached)),
        status: entry.status,
      };
    });
  }, [rows]);

  const retiredCount = useMemo(
    () => mergedRows.filter((r) => r.status === "retired").length,
    [mergedRows]
  );

  const sorted = useMemo(() => {
    const q = query.trim().toLowerCase();
    const filtered = mergedRows.filter((r) => {
      if (!showRetired && r.status === "retired") return false;
      if (!q) return true;
      return (
        r.baseModel.toLowerCase().includes(q) ||
        r.tier.toLowerCase().includes(q) ||
        r.buckets.some((b) => b.windowType.toLowerCase().includes(q))
      );
    });

    const dir = sortAsc ? 1 : -1;
    return [...filtered].sort((a, b) => {
      switch (sortKey) {
        case "usd":
          return dir * (a.totalApiUsd - b.totalApiUsd);
        case "in":
          return dir * (a.maxPctIn - b.maxPctIn);
        case "out":
          return dir * (a.maxPctOut - b.maxPctOut);
        case "cached":
          return dir * (a.maxPctCached - b.maxPctCached);
        case "model":
          return dir * a.baseModel.localeCompare(b.baseModel);
      }
    });
  }, [mergedRows, query, showRetired, sortKey, sortAsc]);

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
        <div className="panel-body">
          <div className="empty-state">
            No model records match the current filter.
          </div>
        </div>
      ) : (
        <div className="table-wrap">
          <table className="data-table">
            <thead>
              <tr>
                <th
                  onClick={() => toggleSort("model")}
                  style={{ cursor: "pointer" }}
                >
                  Model
                </th>
                <th
                  onClick={() => toggleSort("in")}
                  style={{ cursor: "pointer" }}
                >
                  %/1M In
                </th>
                <th
                  onClick={() => toggleSort("out")}
                  style={{ cursor: "pointer" }}
                >
                  %/1M Out
                </th>
                <th
                  onClick={() => toggleSort("cached")}
                  style={{ cursor: "pointer" }}
                >
                  %/1M Cached
                </th>
                <th
                  onClick={() => toggleSort("usd")}
                  style={{ cursor: "pointer" }}
                >
                  Observed Usage
                </th>
              </tr>
            </thead>
            <tbody>
              {sorted.map((r) => {
                const variantLabel =
                  r.variants.length > 0 ? ` (${r.variants.join(", ")})` : "";
                return (
                  <tr
                    key={`${r.tier}-${r.baseModel}`}
                    style={{ opacity: r.status === "retired" ? 0.55 : 1 }}
                  >
                    <td>
                      <div style={{ fontWeight: 600 }}>
                        {r.baseModel}
                        {variantLabel && (
                          <span
                            className="mono"
                            style={{
                              fontSize: 11,
                              fontWeight: 400,
                              opacity: 0.65,
                            }}
                          >
                            {variantLabel}
                          </span>
                        )}
                      </div>
                      <div
                        className="mono"
                        style={{ fontSize: 11, opacity: 0.7 }}
                      >
                        {r.priced
                          ? `$${r.usdIn}/$${r.usdOut}/$${r.usdCached}`
                          : "unpriced"}
                      </div>
                    </td>
                    <td className="mono" style={{ fontSize: 11 }}>
                      {r.buckets.map((b) => (
                        <div key={b.windowType}>
                          <span style={{ opacity: 0.65 }}>
                            {shortBucket(b.windowType)}:
                          </span>{" "}
                          {b.pctIn.toFixed(2)}%
                        </div>
                      ))}
                    </td>
                    <td className="mono" style={{ fontSize: 11 }}>
                      {r.buckets.map((b) => (
                        <div key={b.windowType}>
                          <span style={{ opacity: 0.65 }}>
                            {shortBucket(b.windowType)}:
                          </span>{" "}
                          {b.pctOut.toFixed(2)}%
                        </div>
                      ))}
                    </td>
                    <td className="mono" style={{ fontSize: 11 }}>
                      {r.buckets.map((b) => (
                        <div key={b.windowType}>
                          <span style={{ opacity: 0.65 }}>
                            {shortBucket(b.windowType)}:
                          </span>{" "}
                          {b.pctCached.toFixed(3)}%
                        </div>
                      ))}
                    </td>
                    <td className="mono" style={{ fontSize: 11 }}>
                      <div>
                        ${r.totalApiUsd.toFixed(2)} ({r.totalRequests} reqs)
                      </div>
                      <div style={{ opacity: 0.7 }}>
                        {formatTokens(r.totalTokens)} tok
                      </div>
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
