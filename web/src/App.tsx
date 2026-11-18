import { useCallback, useEffect, useMemo, useState } from "react";
import { CostMetric, TimelineChart } from "./components/TimelineChart";
import { ModelTable, ModelTableRow } from "./components/ModelTable";
import { Predictor } from "./components/Predictor";
import { HoverCard } from "./components/HoverCard";
import {
  ChangeEvent,
  HistorySnapshotPoint,
  PartitionSummary,
  QuotaStatusResponse,
  WindowEstimateView,
} from "./types";

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
  const [metric, setMetric] = useState<CostMetric>("blended");

  const [quotaStatus, setQuotaStatus] = useState<QuotaStatusResponse | null>(null);
  const [history, setHistory] = useState<HistorySnapshotPoint[]>([]);
  const [windowsByTier, setWindowsByTier] = useState<
    { tier: string; view: WindowEstimateView }[]
  >([]);
  const [lastUpdated, setLastUpdated] = useState<number | null>(null);
  const [onlyCostEvents, setOnlyCostEvents] = useState(false);

  const tiersForProvider = useMemo(
    () =>
      Array.from(
        new Set(
          partitions.filter((p) => p.provider === provider).map((p) => p.tier)
        )
      ).sort(),
    [partitions]
  );

  const bucketsForSelection = useMemo(
    () =>
      Array.from(
        new Set(
          partitions
            .filter(
              (p) =>
                p.provider === provider &&
                (tier === "all" || p.tier === tier)
            )
            .map((p) => p.window_type)
        )
      ).sort(),
    [partitions, tier]
  );

  const refreshData = useCallback(async () => {
    try {
      const [quotaResp, partResp] = await Promise.all([
        fetch("/v1/quota"),
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

  const modelRows: ModelTableRow[] = useMemo(
    () =>
      windowsByTier.flatMap(({ tier: t, view }) =>
        view.models.map((m) => ({
          tier: t,
          windowType: view.window_type,
          mix: m.token_mix ?? view.token_mix,
          sigmaHat: view.sigma_hat,
          model: m,
        }))
      ),
    [windowsByTier]
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

  const recentEvents = useMemo(() => {
    const map = new Map<string, ChangeEvent>();
    for (const { view } of windowsByTier) {
      for (const ev of view.recent_change_events) {
        if (onlyCostEvents && ev.event_type !== "cost_change") continue;
        map.set(`${ev.timestamp}-${ev.event_type}-${ev.description}`, ev);
      }
    }
    return Array.from(map.values())
      .sort((a, b) => b.timestamp - a.timestamp)
      .slice(0, 30);
  }, [windowsByTier, onlyCostEvents]);

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
                    {formatResetCountdown(b.reset_time)}
                  </div>
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

      <TimelineChart points={history} metric={metric} onMetricChange={setMetric} />

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
          <span className="panel-title">Change Events & Cost Shifts</span>
          <div className="controls">
            <button
              type="button"
              className={`btn btn-sm ${!onlyCostEvents ? "active" : ""}`}
              onClick={() => setOnlyCostEvents(false)}
            >
              All
            </button>
            <button
              type="button"
              className={`btn btn-sm ${onlyCostEvents ? "active" : ""}`}
              onClick={() => setOnlyCostEvents(true)}
            >
              Cost shifts
            </button>
          </div>
        </div>
        {recentEvents.length === 0 ? (
          <div className="empty">No change events recorded yet.</div>
        ) : (
          <div className="events-list">
            {recentEvents.map((ev, idx) => (
              <div
                className="event-item"
                key={`${ev.timestamp}-${ev.event_type}-${idx}`}
              >
                <span className="event-time">
                  {new Date(ev.timestamp * 1000).toLocaleString()}
                </span>
                <span className="event-desc">{ev.description}</span>
                {ev.p_value !== undefined && ev.p_value !== null && (
                  <span className="event-meta">
                    p={ev.p_value.toExponential(2)}
                  </span>
                )}
              </div>
            ))}
          </div>
        )}
      </section>
    </div>
  );
}
