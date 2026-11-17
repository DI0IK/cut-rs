import { ModelWeightEstimate, TokenMix } from "../types";

interface Props {
  rows: { tier: string; windowType: string; mix: TokenMix; model: ModelWeightEstimate }[];
}

export function ModelTable({ rows }: Props) {
  return (
    <div className="panel" style={{ marginTop: 0 }}>
      <div className="panel-head">
        <span className="panel-title">Active Model Weights (% per 1M tokens)</span>
        <span className="mono" style={{ fontSize: 11, color: "var(--ink-muted)" }}>
          95% CI shown below estimate
        </span>
      </div>
      <div className="table-wrap">
        {rows.length === 0 ? (
          <div className="empty-state">No active model estimates yet.</div>
        ) : (
          <table>
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
              {rows.map(({ tier, windowType, mix, model }) => {
                const blended =
                  mix.in_share * model.pct_per_1m_in +
                  mix.out_share * model.pct_per_1m_out +
                  mix.cached_share * model.pct_per_1m_cached_in;
                return (
                  <tr key={`${tier}-${windowType}-${model.model}`}>
                    <td>
                      <strong className="mono" style={{ fontSize: 12 }}>
                        {model.model}
                      </strong>
                      <span className="ci-sub mono">
                        n={model.sample_count} · {tier}
                      </span>
                    </td>
                    <td className="mono" style={{ fontSize: 12 }}>
                      {windowType}
                    </td>
                    <td className="num">
                      <strong>{blended.toFixed(2)}%</strong>
                    </td>
                    <td className="num">
                      {model.pct_per_1m_in.toFixed(2)}%
                      <span className="ci-sub">
                        [{model.pct_per_1m_in_ci.lower.toFixed(2)}–
                        {model.pct_per_1m_in_ci.upper.toFixed(2)}]
                      </span>
                    </td>
                    <td className="num">
                      {model.pct_per_1m_out.toFixed(2)}%
                      <span className="ci-sub">
                        [{model.pct_per_1m_out_ci.lower.toFixed(2)}–
                        {model.pct_per_1m_out_ci.upper.toFixed(2)}]
                      </span>
                    </td>
                    <td className="num">
                      {model.pct_per_1m_cached_in.toFixed(2)}%
                      <span className="ci-sub">
                        [{model.pct_per_1m_cached_in_ci.lower.toFixed(2)}–
                        {model.pct_per_1m_cached_in_ci.upper.toFixed(2)}]
                      </span>
                    </td>
                    <td>
                      <span className={`badge ${model.identifiability}`}>
                        {model.identifiability}
                      </span>
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
