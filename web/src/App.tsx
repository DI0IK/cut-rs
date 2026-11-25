import { useCallback, useEffect, useMemo, useState } from "react";
import { CostMetric, TimelineChart } from "./components/TimelineChart";
import { QuotaBurndownChart } from "./components/QuotaBurndownChart";
import { ModelTable, ModelTableRow } from "./components/ModelTable";
import { Predictor } from "./components/Predictor";
import { HoverCard } from "./components/HoverCard";
import {
  CurrentBucketStatus,
  HistorySnapshotPoint,
  PartitionSummary,
  QuotaChangeEvent,
  QuotaSnapshotRow,
  QuotaStatusResponse,
  WindowEstimateView,
} from "./types";

interface PaceBudget {
  periodLabel: "day" | "hour";
  unitsLeft: number;
  unitSuffix: "d" | "h";
  startQuotaPct: number;
  budgetPct: number;
  usedPct: number;
  usedRatioPct: number;
}

function computePaceBudget(
  bucket: CurrentBucketStatus,
  history: QuotaSnapshotRow[]
): PaceBudget | null {
  if (bucket.remaining_pct >= 99.999) return null;
  if (!bucket.reset_time) return null;
  const resetMs = Date.parse(bucket.reset_time);
  if (Number.isNaN(resetMs)) return null;

  const lowerId = bucket.bucket_id.toLowerCase();
  const isWeekly = lowerId.includes("weekly") || lowerId.includes("week");
  const isFiveH = lowerId.includes("5h");
  if (!isWeekly && !isFiveH) return null;

  const now = new Date();
  const periodStartSec = isWeekly
    ? Math.floor(
        new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime() /
          1000
      )
    : Math.floor(
        new Date(
          now.getFullYear(),
          now.getMonth(),
          now.getDate(),
          now.getHours()
        ).getTime() / 1000
      );

  const unitSec = isWeekly ? 86400 : 3600;
  const windowDurationSec = isWeekly ? 7 * 86400 : 5 * 3600;
  const nowSec = Math.floor(now.getTime() / 1000);
  const resetSec = Math.floor(resetMs / 1000);
  if (resetSec <= nowSec) return null;

  const windowStartSec = resetSec - windowDurationSec;
  const unitsLeft = Math.max(1, Math.ceil((resetSec - nowSec) / unitSec));

  let startQuotaPct = 100.0;
  if (windowStartSec < periodStartSec) {
    const bucketHist = history.filter((h) => h.bucket_id === bucket.bucket_id);
    let beforePeriod: QuotaSnapshotRow | undefined;
    for (const row of bucketHist) {
      if (row.recorded_at <= periodStartSec) {
        beforePeriod = row;
      }
    }
    if (
      beforePeriod &&
      beforePeriod.reset_time === bucket.reset_time &&
      beforePeriod.remaining_pct >= bucket.remaining_pct - 1e-3
    ) {
      startQuotaPct = beforePeriod.remaining_pct;
    }
  }

  const budgetPct = startQuotaPct / unitsLeft;
  const usedPct = Math.max(0, startQuotaPct - bucket.remaining_pct);
  const usedRatioPct = budgetPct > 0 ? (usedPct / budgetPct) * 100 : 0;

  return {
    periodLabel: isWeekly ? "day" : "hour",
    unitsLeft,
    unitSuffix: isWeekly ? "d" : "h",
    startQuotaPct,
    budgetPct,
    usedPct,
    usedRatioPct,
  };
}

function formatResetCountdown(resetIso: string | null): string {
  if (!resetIso) return "reset unknown";
  const target = Date.parse(resetIso);
  if (Number.isNaN(target)) return `resets ${resetIso}`;
  const secs = Math.floor((target - Date.now()) / 1000);
  if (secs <= 0) return "resetting now";
  const days = Math.floor(secs / 86400);
  const hours = Math.floor((secs % 86400) / 3600);
  const mins = Math.floor((secs % 3600) / 60);
  if (days > 0) return `resets in ${days}d ${hours}h`;
  if (hours > 0) return `resets in ${hours}h ${mins}m`;
  return `resets in ${mins}m`;
}

export function App() {
  const provider = "antigravity";
  const [partitions, setPartitions] = useState<PartitionSummary[]>([]);
  const [tier, setTier] = useState<string>("all");
  const [windowType, setWindowType] = useState<string>("all");
  const [metric, setMetric] = useState<CostMetric>("equiv");

  const [quotaStatus, setQuotaStatus] = useState<QuotaStatusResponse | null>(null);
  const [history, setHistory] = useState<HistorySnapshotPoint[]>([]);
  const [windowsByTier, setWindowsByTier] = useState<
    { tier: string; view: WindowEstimateView }[]
  >([]);
  const [lastUpdated, setLastUpdated] = useState<number | null>(null);
  const [onlyResets, setOnlyResets] = useState(false);

  const activeBucketIds = useMemo(() => {
    if (!quotaStatus || quotaStatus.buckets.length === 0) return null;
    return new Set(quotaStatus.buckets.map((b) => b.bucket_id.toLowerCase()));
  }, [quotaStatus]);

  const activePartitions = useMemo(() => {
    if (!activeBucketIds) return partitions;
    return partitions.filter((p) => activeBucketIds.has(p.window_type.toLowerCase()));
  }, [partitions, activeBucketIds]);

  const tiersForProvider = useMemo(
    () =>
      Array.from(
        new Set(
          activePartitions.filter((p) => p.provider === provider).map((p) => p.tier)
        )
      ).sort(),
    [activePartitions]
  );

  const bucketsForSelection = useMemo(
    () =>
      Array.from(
        new Set(
          activePartitions
            .filter(
              (p) =>
                p.provider === provider &&
                (tier === "all" || p.tier === tier)
            )
            .map((p) => p.window_type)
        )
      ).sort(),
    [activePartitions, tier]
  );

  const refreshData = useCallback(async () => {
    try {
      const [quotaResp, partResp] = await Promise.all([
        fetch("/v1/quota?limit=2000"),
        fetch("/v1/estimates"),
      ]);

      if (quotaResp.ok) {
        const qData = (await quotaResp.json()) as QuotaStatusResponse;
        setQuotaStatus(qData);
      }

      if (!partResp.ok) return;
      const partData = (await partResp.json()) as {
        partitions: PartitionSummary[];
      };
      setPartitions(partData.partitions);

      if (partData.partitions.length === 0) {
        setHistory([]);
        setWindowsByTier([]);
        setLastUpdated(Date.now());
        return;
      }

      const wtQuery =
        windowType && windowType !== "all"
          ? `?window_type=${encodeURIComponent(windowType)}`
          : "";

      const histResp = await fetch(
        `/v1/estimates/${encodeURIComponent(provider)}/${encodeURIComponent(
          tier
        )}/history${wtQuery}`
      );
      if (histResp.ok) {
        const histData = (await histResp.json()) as {
          points: HistorySnapshotPoint[];
        };
        setHistory(histData.points);
      }

      const targetTiers =
        tier === "all"
          ? Array.from(
              new Set(
                partData.partitions
                  .filter((p) => p.provider === provider)
                  .map((p) => p.tier)
              )
            )
          : [tier];

      const allViews: { tier: string; view: WindowEstimateView }[] = [];
      for (const t of targetTiers) {
        const estResp = await fetch(
          `/v1/estimates/${encodeURIComponent(provider)}/${encodeURIComponent(
            t
          )}${wtQuery}`
        );
        if (estResp.ok) {
          const estData = (await estResp.json()) as {
            windows: WindowEstimateView[];
          };
          for (const w of estData.windows) {
            allViews.push({ tier: t, view: w });
          }
        }
      }
      setWindowsByTier(allViews);
      setLastUpdated(Date.now());
    } catch {
      // Transient network error; next interval will retry
    }
  }, [tier, windowType]);

  useEffect(() => {
    refreshData();
    const id = setInterval(refreshData, 15_000);
    return () => clearInterval(id);
  }, [refreshData]);

  const activeHistory = useMemo(() => {
    if (!activeBucketIds) return history;
    return history.filter((h) => activeBucketIds.has(h.window_type.toLowerCase()));
  }, [history, activeBucketIds]);

  const activeQuotaHistory = useMemo(() => {
    const list = quotaStatus?.history ?? [];
    if (!activeBucketIds) return list;
    return list.filter((h) => activeBucketIds.has(h.bucket_id.toLowerCase()));
  }, [quotaStatus, activeBucketIds]);

  const modelRows: ModelTableRow[] = useMemo(
    () =>
      windowsByTier.flatMap(({ tier: t, view }) => {
        if (activeBucketIds && !activeBucketIds.has(view.window_type.toLowerCase())) {
          return [];
        }
        return view.models.map((m) => ({
          tier: t,
          windowType: view.window_type,
          mix: view.token_mix,
          pctPerUsd: view.pct_per_usd,
          bucketApiEquivUsd: view.bucket_api_equivalent_usd,
          model: m,
        }));
      }),
    [windowsByTier, activeBucketIds]
  );

  const uniqueModelNames = useMemo(() => {
    const active = modelRows
      .filter((r) => r.model.status === "active")
      .map((r) => r.model.model);
    const pool = active.length > 0 ? active : modelRows.map((r) => r.model.model);
    return Array.from(new Set(pool)).sort();
  }, [modelRows]);

  const totalSamples = useMemo(
    () => windowsByTier.reduce((acc, w) => acc + w.view.sample_count_window, 0),
    [windowsByTier]
  );

  const avgMix = useMemo(() => {
    if (windowsByTier.length === 0) {
      return { in_share: 0.05, out_share: 0.05, cached_share: 0.9 };
    }
    let wSum = 0;
    let inS = 0;
    let outS = 0;
    let cS = 0;
    for (const { view } of windowsByTier) {
      const n = Math.max(1, view.sample_count_window);
      wSum += n;
      inS += n * view.token_mix.in_share;
      outS += n * view.token_mix.out_share;
      cS += n * view.token_mix.cached_share;
    }
    return {
      in_share: inS / wSum,
      out_share: outS / wSum,
      cached_share: cS / wSum,
    };
  }, [windowsByTier]);

  const recentChanges: QuotaChangeEvent[] = useMemo(() => {
    const list = quotaStatus?.changes ?? [];
    return [...list]
      .filter((ev) => !activeBucketIds || activeBucketIds.has(ev.bucket_id.toLowerCase()))
      .filter((ev) => !onlyResets || ev.event_type === "reset")
      .reverse()
      .slice(0, 30);
  }, [quotaStatus, onlyResets, activeBucketIds]);

  return (
    <div className="shell">
      <header className="topbar">
        <div className="brand">
          <h1>cut-rs</h1>
          <span>Antigravity Local Quota & Cost Tracker</span>
        </div>

        <div className="controls">
          <select
            className="select"
            value={tier}
            onChange={(e) => {
              setTier(e.target.value);
              setWindowType("all");
            }}
          >
            <option value="all">all tiers</option>
            {tiersForProvider.map((t) => (
              <option key={t} value={t}>
                {t}
              </option>
            ))}
          </select>

          <select
            className="select"
            value={windowType}
            onChange={(e) => setWindowType(e.target.value)}
          >
            <option value="all">all buckets</option>
            {bucketsForSelection.map((b) => (
              <option key={b} value={b}>
                {b}
              </option>
            ))}
          </select>

          <button type="button" className="btn" onClick={refreshData}>
            Refresh
          </button>

          <a
            className="btn"
            href="https://github.com/DI0IK/cut-rs"
            target="_blank"
            rel="noreferrer"
            style={{ textDecoration: "none" }}
          >
            GitHub
          </a>
        </div>
      </header>

      <section className="panel">
        <div className="panel-head">
          <span className="panel-title">
            Current Quota Status
            {quotaStatus?.tier ? ` (${quotaStatus.tier})` : ""}
          </span>
          <span style={{ fontSize: 12, opacity: 0.7 }}>
            {quotaStatus?.history.length
              ? `${quotaStatus.history.length} historical snapshots`
              : "local state"}
          </span>
        </div>
        {quotaStatus && quotaStatus.buckets.length > 0 ? (
          <div
            style={{
              display: "grid",
              gridTemplateColumns: "repeat(auto-fit, minmax(220px, 1fr))",
              gap: 12,
              padding: "8px 0",
            }}
          >
            {quotaStatus.buckets.map((b) => {
              const pct = Math.max(0, Math.min(100, b.remaining_pct));
              const barColor =
                pct < 20 ? "#ef4444" : pct < 50 ? "#f59e0b" : "#22c55e";
              const pace = computePaceBudget(b, quotaStatus.history);
              const paceBarPct = pace
                ? Math.max(0, Math.min(100, pace.usedRatioPct))
                : 0;
              const paceColor = pace
                ? pace.usedRatioPct >= 100
                  ? "#ef4444"
                  : pace.usedRatioPct >= 75
                  ? "#f59e0b"
                  : "#38bdf8"
                : "#38bdf8";

              return (
                <div
                  key={b.bucket_id}
                  style={{
                    border: "1px solid var(--border, #2a2f3a)",
                    borderRadius: 8,
                    padding: "10px 12px",
                    background: "rgba(255,255,255,0.02)",
                  }}
                >
                  <div
                    style={{
                      display: "flex",
                      justifyContent: "space-between",
                      alignItems: "baseline",
                      marginBottom: 6,
                    }}
                  >
                    <strong style={{ fontSize: 13 }}>{b.bucket_id}</strong>
                    <span style={{ fontSize: 14, fontWeight: 600 }}>
                      {pct.toFixed(1)}%
                    </span>
                  </div>
                  <div
                    style={{
                      height: 6,
                      borderRadius: 3,
                      background: "rgba(255,255,255,0.08)",
                      overflow: "hidden",
                      marginBottom: 6,
                    }}
                  >
                    <div
                      style={{
                        width: `${pct}%`,
                        height: "100%",
                        background: barColor,
                        transition: "width 0.3s ease",
                      }}
                    />
                  </div>
                  <div style={{ fontSize: 11, opacity: 0.75 }}>
                    {pct >= 99.999
                      ? "Not started"
                      : formatResetCountdown(b.reset_time)}
                  </div>
                  {(() => {
                    const part = partitions.find(
                      (p) =>
                        p.window_type === b.bucket_id.toLowerCase() &&
                        (tier === "all" || p.tier === tier)
                    );
                    if (!part || part.bucket_api_equivalent_usd <= 0) return null;
                    return (
                      <div
                        className="mono"
                        style={{ fontSize: 11, marginTop: 4, opacity: 0.85 }}
                      >
                        100% ≈ ${part.bucket_api_equivalent_usd.toFixed(2)} API (
                        {part.pct_per_usd.toFixed(2)}%/USD)
                      </div>
                    );
                  })()}

                  {pace && (
                    <div
                      style={{
                        marginTop: 10,
                        paddingTop: 8,
                        borderTop: "1px solid rgba(255,255,255,0.07)",
                      }}
                    >
                      <div
                        style={{
                          display: "flex",
                          justifyContent: "space-between",
                          alignItems: "baseline",
                          fontSize: 11,
                          marginBottom: 4,
                        }}
                      >
                        <span style={{ opacity: 0.8 }}>
                          {pace.periodLabel === "day" ? "Today" : "This hour"} (
                          {pace.unitsLeft}
                          {pace.unitSuffix} left)
                        </span>
                        <span style={{ fontWeight: 600 }}>
                          {pace.usedPct.toFixed(1)}% / {pace.budgetPct.toFixed(1)}%
                        </span>
                      </div>
                      <div
                        style={{
                          height: 5,
                          borderRadius: 3,
                          background: "rgba(255,255,255,0.08)",
                          overflow: "hidden",
                          marginBottom: 4,
                        }}
                      >
                        <div
                          style={{
                            width: `${paceBarPct}%`,
                            height: "100%",
                            background: paceColor,
                            transition: "width 0.3s ease",
                          }}
                        />
                      </div>
                      <div
                        style={{
                          display: "flex",
                          justifyContent: "space-between",
                          fontSize: 10,
                          opacity: 0.65,
                        }}
                      >
                        <span>
                          Start of {pace.periodLabel}:{" "}
                          {pace.startQuotaPct.toFixed(1)}%
                        </span>
                        <span>
                          {pace.usedPct <= pace.budgetPct
                            ? `${(pace.budgetPct - pace.usedPct).toFixed(1)}% left`
                            : `${(pace.usedPct - pace.budgetPct).toFixed(1)}% over`}
                        </span>
                      </div>
                    </div>
                  )}
                </div>
              );
            })}
          </div>
        ) : (
          <div style={{ padding: "8px 0", fontSize: 13, opacity: 0.75 }}>
            No Antigravity quota state found yet. Run <code>cut-rs init</code>{" "}
            and use Antigravity CLI to start recording.
          </div>
        )}
      </section>

      <section className="kpi-strip">
        <div className="kpi">
          <HoverCard
            card={
              <div className="hc-grid">
                <div className="hc-title">Recorded Intervals</div>
                <div className="hc-desc">
                  Total quota-drop intervals in the selected buckets used for on-demand estimation.
                </div>
              </div>
            }
          >
            <div className="kpi-label hc-trigger">Recorded Intervals</div>
          </HoverCard>
          <div className="kpi-val">{totalSamples}</div>
        </div>
        <div className="kpi">
          <HoverCard
            card={
              <div className="hc-grid">
                <div className="hc-title">Token Mix</div>
                <div className="hc-desc">
                  Your observed share of uncached input, output, and cached input tokens used to
                  compute the Blended cost rate.
                </div>
              </div>
            }
          >
            <div className="kpi-label hc-trigger">Token Mix</div>
          </HoverCard>
          <div className="kpi-val" style={{ fontSize: 13 }}>
            {(avgMix.in_share * 100).toFixed(0)}% in ·{" "}
            {(avgMix.out_share * 100).toFixed(0)}% out ·{" "}
            {(avgMix.cached_share * 100).toFixed(1)}% cached
          </div>
        </div>
        <div className="kpi">
          <div className="kpi-label">Last Refresh</div>
          <div className="kpi-val" style={{ fontSize: 13 }}>
            {lastUpdated ? new Date(lastUpdated).toLocaleTimeString() : "--"}
          </div>
        </div>
      </section>

      <QuotaBurndownChart history={activeQuotaHistory} />

      <TimelineChart points={activeHistory} metric={metric} onMetricChange={setMetric} />

      <div className="grid-2">
        <ModelTable rows={modelRows} />
        <Predictor
          provider={provider}
          tier={tier}
          windowType={windowType}
          models={uniqueModelNames}
        />
      </div>

      <section className="panel">
        <div className="panel-head">
          <span className="panel-title">Quota Changes & Resets</span>
          <div className="controls">
            <button
              type="button"
              className={`btn btn-sm ${!onlyResets ? "active" : ""}`}
              onClick={() => setOnlyResets(false)}
            >
              All
            </button>
            <button
              type="button"
              className={`btn btn-sm ${onlyResets ? "active" : ""}`}
              onClick={() => setOnlyResets(true)}
            >
              Resets only
            </button>
          </div>
        </div>
        {recentChanges.length === 0 ? (
          <div className="panel-body">
            <div className="empty-state">No quota changes recorded yet.</div>
          </div>
        ) : (
          <div className="table-wrap" style={{ maxHeight: 280, overflowY: "auto" }}>
            <table className="data-table">
              <thead>
                <tr>
                  <th>Time</th>
                  <th>Bucket</th>
                  <th>Type</th>
                  <th>Change</th>
                  <th>Delta</th>
                  <th>Next Reset</th>
                </tr>
              </thead>
              <tbody>
                {recentChanges.map((ev) => {
                  const isReset = ev.event_type === "reset";
                  const deltaStr =
                    ev.delta_pct !== null
                      ? `${ev.delta_pct >= 0 ? "+" : ""}${ev.delta_pct.toFixed(2)}%`
                      : "--";
                  const resetFormatted = ev.reset_time
                    ? new Date(ev.reset_time).toLocaleString([], {
                        month: "short",
                        day: "numeric",
                        hour: "2-digit",
                        minute: "2-digit",
                      })
                    : "--";
                  return (
                    <tr key={ev.id}>
                      <td className="mono" style={{ fontSize: 11, color: "var(--ink-muted)" }}>
                        {new Date(ev.recorded_at * 1000).toLocaleString([], {
                          month: "short",
                          day: "numeric",
                          hour: "2-digit",
                          minute: "2-digit",
                          second: "2-digit",
                        })}
                      </td>
                      <td className="mono" style={{ fontWeight: 600 }}>
                        {ev.bucket_id}
                      </td>
                      <td>
                        <span
                          className="mono"
                          style={{
                            fontSize: 10,
                            padding: "2px 6px",
                            borderRadius: 3,
                            background: isReset
                              ? "rgba(21, 128, 61, 0.12)"
                              : "var(--surface-muted)",
                            color: isReset ? "var(--ok)" : "var(--ink-muted)",
                            textTransform: "uppercase",
                          }}
                        >
                          {ev.event_type}
                        </span>
                      </td>
                      <td className="mono" style={{ fontSize: 12 }}>
                        {ev.prev_pct !== null ? `${ev.prev_pct.toFixed(2)}% → ` : ""}
                        <strong>{ev.curr_pct.toFixed(2)}%</strong>
                      </td>
                      <td
                        className="mono"
                        style={{
                          fontSize: 12,
                          color:
                            ev.delta_pct !== null && ev.delta_pct > 0
                              ? "var(--ok)"
                              : "var(--ink)",
                        }}
                      >
                        {deltaStr}
                      </td>
                      <td className="mono" style={{ fontSize: 11, color: "var(--ink-muted)" }}>
                        {resetFormatted}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </section>
    </div>
  );
}
