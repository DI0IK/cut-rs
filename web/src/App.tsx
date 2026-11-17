import { useCallback, useEffect, useMemo, useState } from "react";
import { CostMetric, TimelineChart } from "./components/TimelineChart";
import { ModelTable } from "./components/ModelTable";
import { Predictor } from "./components/Predictor";
import {
  ChangeEvent,
  HistorySnapshotPoint,
  PartitionSummary,
  WindowEstimateView,
} from "./types";

export function App() {
  const [partitions, setPartitions] = useState<PartitionSummary[]>([]);
  const [provider, setProvider] = useState<string>("");
  const [tier, setTier] = useState<string>("all");
  const [windowType, setWindowType] = useState<string>("all");
  const [metric, setMetric] = useState<CostMetric>("blended");

  const [history, setHistory] = useState<HistorySnapshotPoint[]>([]);
  const [windowsByTier, setWindowsByTier] = useState<
    { tier: string; view: WindowEstimateView }[]
  >([]);
  const [lastUpdated, setLastUpdated] = useState<number | null>(null);
  const [showSetup, setShowSetup] = useState(false);
  const [copiedCmd, setCopiedCmd] = useState<string | null>(null);

  const copyCommand = (id: string, text: string) => {
    navigator.clipboard?.writeText(text);
    setCopiedCmd(id);
    setTimeout(() => setCopiedCmd(null), 1500);
  };

  const providers = useMemo(
    () => Array.from(new Set(partitions.map((p) => p.provider))).sort(),
    [partitions]
  );

  const tiersForProvider = useMemo(
    () =>
      Array.from(
        new Set(
          partitions.filter((p) => p.provider === provider).map((p) => p.tier)
        )
      ).sort(),
    [partitions, provider]
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
    [partitions, provider, tier]
  );

  const refreshData = useCallback(async () => {
    try {
      const partResp = await fetch("/v1/estimates");
      if (!partResp.ok) return;
      const partData = (await partResp.json()) as {
        partitions: PartitionSummary[];
      };
      setPartitions(partData.partitions);

      const activeProvider =
        provider || partData.partitions[0]?.provider || "";
      if (!provider && activeProvider) {
        setProvider(activeProvider);
      }
      if (!activeProvider) return;

      const wtQuery =
        windowType && windowType !== "all"
          ? `?window_type=${encodeURIComponent(windowType)}`
          : "";

      const histResp = await fetch(
        `/v1/estimates/${encodeURIComponent(activeProvider)}/${encodeURIComponent(
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
                  .filter((p) => p.provider === activeProvider)
                  .map((p) => p.tier)
              )
            )
          : [tier];

      const allViews: { tier: string; view: WindowEstimateView }[] = [];
      for (const t of targetTiers) {
        const estResp = await fetch(
          `/v1/estimates/${encodeURIComponent(activeProvider)}/${encodeURIComponent(
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
  }, [provider, tier, windowType]);

  useEffect(() => {
    refreshData();
    const id = setInterval(refreshData, 15_000);
    return () => clearInterval(id);
  }, [refreshData]);

  const modelRows = useMemo(
    () =>
      windowsByTier.flatMap(({ tier: t, view }) =>
        view.models.map((m) => ({
          tier: t,
          windowType: view.window_type,
          mix: view.token_mix,
          model: m,
        }))
      ),
    [windowsByTier]
  );

  const uniqueModelNames = useMemo(
    () => Array.from(new Set(modelRows.map((r) => r.model.model))).sort(),
    [modelRows]
  );

  const totalSamples = useMemo(
    () => windowsByTier.reduce((acc, w) => acc + w.view.sample_count_window, 0),
    [windowsByTier]
  );

  const maxContributors = useMemo(
    () =>
      windowsByTier.reduce(
        (acc, w) => Math.max(acc, w.view.effective_contributors),
        0
      ),
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
        map.set(`${ev.timestamp}-${ev.event_type}-${ev.description}`, ev);
      }
    }
    return Array.from(map.values())
      .sort((a, b) => b.timestamp - a.timestamp)
      .slice(0, 12);
  }, [windowsByTier]);

  return (
    <div className="shell">
      <header className="topbar">
        <div className="brand">
          <h1>cut-rs</h1>
          <span>AI Quota Telemetry</span>
        </div>

        <div className="controls">
          <select
            className="select"
            value={provider}
            onChange={(e) => {
              setProvider(e.target.value);
              setTier("all");
              setWindowType("all");
            }}
          >
            {providers.length === 0 ? (
              <option value="">no providers yet</option>
            ) : (
              providers.map((p) => (
                <option key={p} value={p}>
                  {p}
                </option>
              ))
            )}
          </select>

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

          <button
            type="button"
            className="btn btn-accent"
            onClick={() => setShowSetup(true)}
          >
            Setup CLI
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

      {providers.length === 0 && (
        <div className="onboard-banner">
          <span>
            No telemetry data yet. Install the CLI in 30 seconds to contribute
            anonymous quota samples from Claude Code, Codex, or Antigravity CLI.
          </span>
          <button
            type="button"
            className="btn btn-accent"
            onClick={() => setShowSetup(true)}
          >
            Get Started
          </button>
        </div>
      )}

      <section className="kpi-strip">
        <div className="kpi">
          <div className="kpi-label">Window Samples</div>
          <div className="kpi-val">{totalSamples}</div>
        </div>
        <div
          className="kpi"
          title="Effective independent contributors after the 5% per-client weight cap"
        >
          <div className="kpi-label">Eff. Contributors</div>
          <div className="kpi-val">{maxContributors.toFixed(1)}</div>
        </div>
        <div
          className="kpi"
          title="Empirical share of input, output, and cached tokens across recent samples"
        >
          <div className="kpi-label">Crowdsourced Token Mix</div>
          <div className="kpi-val" style={{ fontSize: 13 }}>
            {(avgMix.in_share * 100).toFixed(0)}% in ·{" "}
            {(avgMix.out_share * 100).toFixed(0)}% out ·{" "}
            {(avgMix.cached_share * 100).toFixed(1)}% cached
          </div>
        </div>
        <div className="kpi">
          <div className="kpi-label">Last Sync</div>
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
          <span className="panel-title">Change Events & Regime Shifts</span>
        </div>
        <div className="panel-body">
          {recentEvents.length === 0 ? (
            <div className="empty-state" style={{ padding: 16 }}>
              No change events recorded yet.
            </div>
          ) : (
            <ul className="events-list">
              {recentEvents.map((ev, i) => (
                <li key={`${ev.timestamp}-${i}`}>
                  <span className="mono" style={{ color: "var(--ink-muted)" }}>
                    {new Date(ev.timestamp * 1000).toLocaleString()}
                  </span>
                  <span className="badge">{ev.event_type}</span>
                  <span>{ev.description}</span>
                </li>
              ))}
            </ul>
          )}
        </div>
      </section>

      {showSetup && (
        <div
          className="modal-backdrop"
          role="dialog"
          aria-modal="true"
          onClick={() => setShowSetup(false)}
        >
          <div className="modal-card" onClick={(e) => e.stopPropagation()}>
            <div className="panel-head">
              <span className="panel-title">Contribute Telemetry (60s Setup)</span>
              <button
                type="button"
                className="btn"
                onClick={() => setShowSetup(false)}
              >
                Close
              </button>
            </div>
            <div className="modal-body">
              <p className="modal-lead">
                Supports <strong>Claude Code</strong>, <strong>OpenAI Codex</strong>,{" "}
                <strong>Antigravity CLI</strong>, and <strong>Waybar</strong> using
                only official local interfaces.
              </p>

              <div className="setup-step">
                <div className="setup-label">1. Install binary (Linux / macOS)</div>
                <div className="code-row">
                  <code>
                    curl -fsSL https://raw.githubusercontent.com/DI0IK/cut-rs/main/scripts/install.sh | sh
                  </code>
                  <button
                    type="button"
                    className="btn"
                    onClick={() =>
                      copyCommand(
                        "unix",
                        "curl -fsSL https://raw.githubusercontent.com/DI0IK/cut-rs/main/scripts/install.sh | sh"
                      )
                    }
                  >
                    {copiedCmd === "unix" ? "Copied" : "Copy"}
                  </button>
                </div>
              </div>

              <div className="setup-step">
                <div className="setup-label">1b. Install binary (Windows PowerShell)</div>
                <div className="code-row">
                  <code>
                    irm https://raw.githubusercontent.com/DI0IK/cut-rs/main/scripts/install.ps1 | iex
                  </code>
                  <button
                    type="button"
                    className="btn"
                    onClick={() =>
                      copyCommand(
                        "win",
                        "irm https://raw.githubusercontent.com/DI0IK/cut-rs/main/scripts/install.ps1 | iex"
                      )
                    }
                  >
                    {copiedCmd === "win" ? "Copied" : "Copy"}
                  </button>
                </div>
              </div>

              <div className="setup-step">
                <div className="setup-label">
                  2. Auto-detect tools &amp; configure hooks safely
                </div>
                <div className="code-row">
                  <code>cut-rs init &amp;&amp; cut-rs status</code>
                  <button
                    type="button"
                    className="btn"
                    onClick={() => copyCommand("init", "cut-rs init && cut-rs status")}
                  >
                    {copiedCmd === "init" ? "Copied" : "Copy"}
                  </button>
                </div>
              </div>

              <p className="modal-note">
                Privacy: only token count deltas and quota % changes are submitted.
                Prompts, code, and raw IP addresses are never collected or stored.
                Preexisting <code>statusLine</code> commands are chained automatically.
              </p>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}
