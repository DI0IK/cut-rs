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

export function ModelTable({ rows }: Props) {
  return (
    <div className="panel" style={{ marginTop: 0 }}>
      <div className="panel-head">
        <span className="panel-title">Active Model Weights (% per 1M tokens)</span>
        <span className="mono" style={{ fontSize: 11, color: "var(--ink-muted)" }}>
          Hover values for capacity &amp; diagnostics
        </span>
      </div>
      <div className="table-wrap">
        {rows.length === 0 ? (
          <div className="empty-state">No active model estimates yet.</div>
        ) : (
          <table className="model-table">
            <thead>
              <tr>
                <th>Model</th>
                <th>Bucket</th>
                <th className="num">Blended</th>
                <th className="num">In</th>
                <th className="num">Out</th>
                <th className="num">Cached</th>
                <th>Identifiability</th>
              </tr>
            </thead>
            <tbody>
              {rows.map(({ tier, windowType, mix, sigmaHat, model }) => {
                const blended =
                  mix.in_share * model.pct_per_1m_in +
                  mix.out_share * model.pct_per_1m_out +
                  mix.cached_share * model.pct_per_1m_cached_in;
                const impliedBlendedM = blended > 1e-6 ? 100 / blended : null;
                const impliedOutM =
                  model.pct_per_1m_out > 1e-6 ? 100 / model.pct_per_1m_out : null;
                const isUnidentifiable =
                  model.identifiability === "insufficient_samples" ||
                  model.identifiability === "low_collinear";
                const atOutInBoundary =
                  model.pct_per_1m_in > 1e-6 &&
                  Math.abs(model.pct_per_1m_out - model.pct_per_1m_in) < 1e-4;

                return (
                  <tr key={`${tier}-${windowType}-${model.model}`}>
                    <td data-label="Model">
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
                              <strong>
                                {impliedBlendedM ? `~${impliedBlendedM.toFixed(1)}M tok` : "--"}
                              </strong>
                            </div>
                            <div className="hc-row">
                              <span>100% Window (Out-only)</span>
                              <strong>
                                {impliedOutM ? `~${impliedOutM.toFixed(2)}M tok` : "--"}
                              </strong>
                            </div>
                            <div className="hc-row">
                              <span>Last Seen</span>
                              <span>{new Date(model.last_seen_at * 1000).toLocaleString()}</span>
                            </div>
                          </div>
                        }
                      >
                        <strong className="mono hc-trigger" style={{ fontSize: 12 }}>
                          {model.model}
                        </strong>
                      </HoverCard>
                      <span className="ci-sub mono">
                        n={model.sample_count} · {tier}
                      </span>
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
                              <span>
                                {(mix.cached_share * model.pct_per_1m_cached_in).toFixed(3)}%
                              </span>
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
                        [{model.pct_per_1m_in_ci.lower.toFixed(2)}–
                        {model.pct_per_1m_in_ci.upper.toFixed(2)}]
                      </span>
                    </td>
                    <td data-label="Out" className="num" style={isUnidentifiable ? { opacity: 0.65 } : undefined}>
                      {model.pct_per_1m_out.toFixed(2)}%
                      <span className="ci-sub">
                        [{model.pct_per_1m_out_ci.lower.toFixed(2)}–
                        {model.pct_per_1m_out_ci.upper.toFixed(2)}]
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
                            <div className="hc-desc">
                              {identifiabilityExplanation(model.identifiability)}
                            </div>
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
              })}
            </tbody>
          </table>
        )}
      </div>
    </div>
  );
}
