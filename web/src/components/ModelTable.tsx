import { Fragment, useMemo, useState } from "react";
import { ModelWeightEstimate, TokenMix } from "../types";
import { HoverCard } from "./HoverCard";

export interface ModelTableRow {
  tier: string;
  windowType: string;
  mix: TokenMix;
  sigmaHat: number;
  model: ModelWeightEstimate;
}

interface Props {
  rows: ModelTableRow[];
}

type SortKey = "blended" | "in" | "out" | "cached" | "model";

const DEFAULT_VISIBLE_GROUPS = 12;
const THINKING_SUFFIX_RE = /^(.*?)\s*\((minimal|low|medium|high|xhigh|max|none|auto)\)$/i;

function splitThinkingVariant(name: string): { baseModel: string; variant: string | null } {
  const m = name.trim().match(THINKING_SUFFIX_RE);
  if (m && m[1].trim().length > 0) {
    return { baseModel: m[1].trim(), variant: m[2].toLowerCase() };
  }
  return { baseModel: name.trim(), variant: null };
}

function blendedOf(row: ModelTableRow): number {
  const { mix, model } = row;
  return (
    mix.in_share * model.pct_per_1m_in +
    mix.out_share * model.pct_per_1m_out +
    mix.cached_share * model.pct_per_1m_cached_in
  );
}

function formatRange(vals: number[]): string {
  const min = Math.min(...vals);
  const max = Math.max(...vals);
  if (Math.abs(max - min) < 0.005) return `${min.toFixed(2)}%`;
  return `${min.toFixed(2)}–${max.toFixed(2)}%`;
}

function identifiabilityExplanation(level: ModelWeightEstimate["identifiability"]): string {
  switch (level) {
    case "high":
      return "High: Input, output, and cached weights are well-separated with low collinearity.";
    case "moderate":
      return "Moderate: Blended cost is solid; individual weights are stabilizing as more varied turns arrive.";
    case "low_collinear":
      return "Low (collinear): Tokens move in a similar ratio across turns. Needs more low-cache / cold-start turns to separate In from Cached.";
    case "insufficient_samples":
      return "Insufficient samples (n < 15): Blended cost is usable as a rough guide; individual token weights are still stabilizing.";
  }
}

interface BucketGroup {
  windowType: string;
  items: { row: ModelTableRow; variant: string | null; blended: number }[];
}

interface BaseModelGroup {
  key: string;
  tier: string;
  baseModel: string;
  buckets: BucketGroup[];
  maxBlended: number;
  maxIn: number;
  maxOut: number;
  maxCached: number;
  allRetired: boolean;
}

export function ModelTable({ rows }: Props) {
  const [query, setQuery] = useState("");
  const [showRetired, setShowRetired] = useState(false);
  const [showAllGroups, setShowAllGroups] = useState(false);
  const [expandedBuckets, setExpandedBuckets] = useState<Record<string, boolean>>({});
  const [sortKey, setSortKey] = useState<SortKey>("blended");
  const [sortAsc, setSortAsc] = useState(false);

  const retiredCount = useMemo(
    () => rows.filter((r) => r.model.status === "retired").length,
    [rows]
  );

  const groups = useMemo(() => {
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

    const byBase = new Map<string, BaseModelGroup>();
    for (const row of filtered) {
      const { baseModel, variant } = splitThinkingVariant(row.model.model);
      const groupKey = `${row.tier}\0${baseModel}`;
      let grp = byBase.get(groupKey);
      if (!grp) {
        grp = {
          key: groupKey,
          tier: row.tier,
          baseModel,
          buckets: [],
          maxBlended: 0,
          maxIn: 0,
          maxOut: 0,
          maxCached: 0,
          allRetired: true,
        };
        byBase.set(groupKey, grp);
      }

      const bVal = blendedOf(row);
      grp.maxBlended = Math.max(grp.maxBlended, bVal);
      grp.maxIn = Math.max(grp.maxIn, row.model.pct_per_1m_in);
      grp.maxOut = Math.max(grp.maxOut, row.model.pct_per_1m_out);
      grp.maxCached = Math.max(grp.maxCached, row.model.pct_per_1m_cached_in);
      if (row.model.status !== "retired") grp.allRetired = false;

      let bucket = grp.buckets.find((b) => b.windowType === row.windowType);
      if (!bucket) {
        bucket = { windowType: row.windowType, items: [] };
        grp.buckets.push(bucket);
      }
      bucket.items.push({ row, variant, blended: bVal });
    }

    const list = Array.from(byBase.values());
    for (const grp of list) {
      grp.buckets.sort((a, b) => a.windowType.localeCompare(b.windowType));
      for (const b of grp.buckets) {
        b.items.sort((a, c) => c.row.model.sample_count - a.row.model.sample_count);
      }
    }

    list.sort((a, b) => {
      if (a.allRetired !== b.allRetired) return a.allRetired ? 1 : -1;
      let cmp = 0;
      switch (sortKey) {
        case "model":
          cmp = a.baseModel.localeCompare(b.baseModel);
          break;
        case "blended":
          cmp = a.maxBlended - b.maxBlended;
          break;
        case "in":
          cmp = a.maxIn - b.maxIn;
          break;
        case "out":
          cmp = a.maxOut - b.maxOut;
          break;
        case "cached":
          cmp = a.maxCached - b.maxCached;
          break;
      }
      return sortAsc ? cmp : -cmp;
    });

    return list;
  }, [rows, query, showRetired, sortKey, sortAsc]);

  const visibleGroups = showAllGroups ? groups : groups.slice(0, DEFAULT_VISIBLE_GROUPS);

  const toggleSort = (key: SortKey) => {
    if (sortKey === key) {
      setSortAsc((prev) => !prev);
    } else {
      setSortKey(key);
      setSortAsc(key === "model");
    }
  };

  const sortArrow = (key: SortKey) => {
    if (sortKey !== key) return "";
    return sortAsc ? " ▲" : " ▼";
  };

  const renderSingleRow = (
    row: ModelTableRow,
    opts: {
      rowKey: string;
      showModelHeader: boolean;
      displayName: string;
      subLabel: string;
      indented?: boolean;
    }
  ) => {
    const { tier, windowType, mix, sigmaHat, model } = row;
    const blended = blendedOf(row);
    const impliedBlendedM = blended > 1e-6 ? 100 / blended : null;
    const impliedOutM = model.pct_per_1m_out > 1e-6 ? 100 / model.pct_per_1m_out : null;
    const isUnidentifiable =
      model.identifiability === "insufficient_samples" ||
      model.identifiability === "low_collinear";
    const atOutInBoundary =
      model.pct_per_1m_in > 1e-6 &&
      Math.abs(model.pct_per_1m_out - model.pct_per_1m_in) < 1e-4;
    const isRetired = model.status === "retired";

    return (
      <tr key={opts.rowKey} className={isRetired ? "row-retired" : undefined}>
        <td data-label="Model" className={opts.indented ? "model-indented" : undefined}>
          {opts.showModelHeader ? (
            <>
              <HoverCard
                card={
                  <div className="hc-grid">
                    <div className="hc-title">{model.model}</div>
                    <div className="hc-row">
                      <span>Tier / Samples</span>
                      <strong>
                        {tier} · n={model.sample_count}
                      </strong>
                    </div>
                    <div className="hc-row">
                      <span>100% Window (Blended)</span>
                      <strong>{impliedBlendedM ? `~${impliedBlendedM.toFixed(1)}M tok` : "--"}</strong>
                    </div>
                    <div className="hc-row">
                      <span>100% Window (Out-only)</span>
                      <strong>{impliedOutM ? `~${impliedOutM.toFixed(2)}M tok` : "--"}</strong>
                    </div>
                    <div className="hc-row">
                      <span>Last Seen</span>
                      <span>{new Date(model.last_seen_at * 1000).toLocaleString()}</span>
                    </div>
                  </div>
                }
              >
                <strong className="mono hc-trigger" style={{ fontSize: 12 }}>
                  {opts.displayName}
                </strong>
              </HoverCard>
              {isRetired && <span className="badge retired">retired</span>}
              <span className="ci-sub mono">{opts.subLabel}</span>
            </>
          ) : (
            <span className="ci-sub mono" style={{ marginTop: 0 }}>
              ↳ n={model.sample_count}
            </span>
          )}
        </td>
        <td data-label="Bucket" className="mono" style={{ fontSize: 12 }}>
          {windowType}
        </td>
        <td data-label="Blended" className="num">
          <HoverCard
            align="right"
            card={
              <div className="hc-grid">
                <div className="hc-title">Blended Mix Breakdown</div>
                <div className="hc-row">
                  <span>In ({(mix.in_share * 100).toFixed(1)}%)</span>
                  <span>{(mix.in_share * model.pct_per_1m_in).toFixed(3)}%</span>
                </div>
                <div className="hc-row">
                  <span>Out ({(mix.out_share * 100).toFixed(1)}%)</span>
                  <span>{(mix.out_share * model.pct_per_1m_out).toFixed(3)}%</span>
                </div>
                <div className="hc-row">
                  <span>Cached ({(mix.cached_share * 100).toFixed(1)}%)</span>
                  <span>{(mix.cached_share * model.pct_per_1m_cached_in).toFixed(3)}%</span>
                </div>
                <div className="hc-row">
                  <span>Residual σ̂</span>
                  <span>±{sigmaHat.toFixed(3)}%</span>
                </div>
              </div>
            }
          >
            <strong className="hc-trigger">{blended.toFixed(2)}%</strong>
          </HoverCard>
        </td>
        <td data-label="In" className="num" style={isUnidentifiable ? { opacity: 0.65 } : undefined}>
          {model.pct_per_1m_in.toFixed(2)}%
          <span className="ci-sub">
            [{model.pct_per_1m_in_ci.lower.toFixed(2)}–{model.pct_per_1m_in_ci.upper.toFixed(2)}]
          </span>
        </td>
        <td data-label="Out" className="num" style={isUnidentifiable ? { opacity: 0.65 } : undefined}>
          {model.pct_per_1m_out.toFixed(2)}%
          <span className="ci-sub">
            [{model.pct_per_1m_out_ci.lower.toFixed(2)}–{model.pct_per_1m_out_ci.upper.toFixed(2)}]
          </span>
        </td>
        <td data-label="Cached" className="num" style={isUnidentifiable ? { opacity: 0.65 } : undefined}>
          {model.pct_per_1m_cached_in.toFixed(2)}%
          <span className="ci-sub">
            [{model.pct_per_1m_cached_in_ci.lower.toFixed(2)}–
            {model.pct_per_1m_cached_in_ci.upper.toFixed(2)}]
          </span>
        </td>
        <td data-label="Identifiability">
          <HoverCard
            align="right"
            card={
              <div className="hc-grid">
                <div className="hc-title">Collinearity Diagnostic</div>
                <div className="hc-desc">{identifiabilityExplanation(model.identifiability)}</div>
                {atOutInBoundary && (
                  <div className="hc-desc" style={{ marginTop: 6 }}>
                    Note: Out = In (hit monotonic NNLS boundary w_out &gt;= w_in).
                  </div>
                )}
              </div>
            }
          >
            <span className={`badge hc-trigger ${model.identifiability}`}>
              {model.identifiability}
            </span>
          </HoverCard>
        </td>
      </tr>
    );
  };

  return (
    <div className="panel" style={{ marginTop: 0 }}>
      <div className="panel-head">
        <span className="panel-title">Active Model Weights (% per 1M tokens)</span>
        <input
          type="search"
          className="input-sm mono"
          placeholder="Filter models…"
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          aria-label="Filter models"
        />
      </div>
      <div className="table-wrap">
        {visibleGroups.length === 0 ? (
          <div className="empty-state">
            {rows.length === 0
              ? "No active model estimates yet."
              : "No active models match the current filter."}
          </div>
        ) : (
          <table className="model-table">
            <thead>
              <tr>
                <th>
                  <button type="button" className="th-sort" onClick={() => toggleSort("model")}>
                    Model{sortArrow("model")}
                  </button>
                </th>
                <th>Bucket</th>
                <th className="num">
                  <button type="button" className="th-sort" onClick={() => toggleSort("blended")}>
                    Blended{sortArrow("blended")}
                  </button>
                </th>
                <th className="num">
                  <button type="button" className="th-sort" onClick={() => toggleSort("in")}>
                    In{sortArrow("in")}
                  </button>
                </th>
                <th className="num">
                  <button type="button" className="th-sort" onClick={() => toggleSort("out")}>
                    Out{sortArrow("out")}
                  </button>
                </th>
                <th className="num">
                  <button type="button" className="th-sort" onClick={() => toggleSort("cached")}>
                    Cached{sortArrow("cached")}
                  </button>
                </th>
                <th>Identifiability</th>
              </tr>
            </thead>
            <tbody>
              {visibleGroups.map((grp) =>
                grp.buckets.map((bucket, bIdx) => {
                  const isFirstBucket = bIdx === 0;
                  if (bucket.items.length === 1) {
                    const { row } = bucket.items[0];
                    return renderSingleRow(row, {
                      rowKey: `${grp.key}-${bucket.windowType}-${row.model.model}`,
                      showModelHeader: isFirstBucket,
                      displayName: row.model.model,
                      subLabel: `n=${row.model.sample_count} · ${grp.tier}`,
                    });
                  }

                  const expandKey = `${grp.key}\0${bucket.windowType}`;
                  const isExpanded = Boolean(expandedBuckets[expandKey]);
                  const totalN = bucket.items.reduce((acc, it) => acc + it.row.model.sample_count, 0);
                  const topItem = bucket.items[0];
                  const topIdent = topItem.row.model.identifiability;

                  return (
                    <Fragment key={expandKey}>
                      <tr
                        className="row-group-toggle"
                        onClick={() =>
                          setExpandedBuckets((prev) => ({
                            ...prev,
                            [expandKey]: !prev[expandKey],
                          }))
                        }
                      >
                        <td data-label="Model">
                          <span className="group-chevron mono">{isExpanded ? "▾" : "▸"}</span>
                          {isFirstBucket ? (
                            <>
                              <strong className="mono" style={{ fontSize: 12 }}>
                                {grp.baseModel}
                              </strong>
                              <span className="ci-sub mono">
                                {bucket.items.length} variants · n={totalN} · {grp.tier}
                              </span>
                            </>
                          ) : (
                            <span className="ci-sub mono" style={{ marginTop: 0 }}>
                              {bucket.items.length} variants · n={totalN}
                            </span>
                          )}
                        </td>
                        <td data-label="Bucket" className="mono" style={{ fontSize: 12 }}>
                          {bucket.windowType}
                        </td>
                        <td data-label="Blended" className="num">
                          <strong>{formatRange(bucket.items.map((it) => it.blended))}</strong>
                        </td>
                        <td data-label="In" className="num">
                          {formatRange(bucket.items.map((it) => it.row.model.pct_per_1m_in))}
                        </td>
                        <td data-label="Out" className="num">
                          {formatRange(bucket.items.map((it) => it.row.model.pct_per_1m_out))}
                        </td>
                        <td data-label="Cached" className="num">
                          {formatRange(bucket.items.map((it) => it.row.model.pct_per_1m_cached_in))}
                        </td>
                        <td data-label="Identifiability">
                          <span className={`badge ${topIdent}`}>{topIdent}</span>
                        </td>
                      </tr>
                      {isExpanded &&
                        bucket.items.map(({ row, variant }) =>
                          renderSingleRow(row, {
                            rowKey: `${expandKey}-${row.model.model}`,
                            showModelHeader: true,
                            displayName: variant ? `(${variant})` : row.model.model,
                            subLabel: `n=${row.model.sample_count}`,
                            indented: true,
                          })
                        )}
                    </Fragment>
                  );
                })
              )}
            </tbody>
          </table>
        )}
      </div>
      {(retiredCount > 0 || groups.length > DEFAULT_VISIBLE_GROUPS) && (
        <div className="table-footer">
          {groups.length > DEFAULT_VISIBLE_GROUPS && (
            <button
              type="button"
              className="btn btn-sm"
              onClick={() => setShowAllGroups((v) => !v)}
            >
              {showAllGroups ? `Show top ${DEFAULT_VISIBLE_GROUPS}` : `Show all ${groups.length} models`}
            </button>
          )}
          {retiredCount > 0 && (
            <button
              type="button"
              className="btn btn-sm"
              onClick={() => setShowRetired((v) => !v)}
            >
              {showRetired ? "Hide retired" : `Show ${retiredCount} retired`}
            </button>
          )}
        </div>
      )}
    </div>
  );
}
