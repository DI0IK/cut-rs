import { useEffect, useState } from "react";
import { PredictResponse } from "../types";

interface Props {
  provider: string;
  tier: string;
  windowType: string;
  models: string[];
}

export function Predictor({ provider, tier, windowType, models }: Props) {
  const [model, setModel] = useState(models[0] ?? "");
  const [tokensIn, setTokensIn] = useState(20_000);
  const [tokensOut, setTokensOut] = useState(5_000);
  const [tokensCached, setTokensCached] = useState(200_000);
  const [result, setResult] = useState<PredictResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  useEffect(() => {
    if (models.length > 0 && !models.includes(model)) {
      setModel(models[0]);
    }
  }, [models, model]);

  useEffect(() => {
    if (!provider || !tier || tier === "all" || !model) {
      setResult(null);
      return;
    }

    const controller = new AbortController();
    const timer = setTimeout(async () => {
      try {
        setError(null);
        const resp = await fetch(
          `/v1/estimates/${encodeURIComponent(provider)}/${encodeURIComponent(tier)}/predict`,
          {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            signal: controller.signal,
            body: JSON.stringify({
              window_type: windowType === "all" ? undefined : windowType,
              usage: [
                {
                  model,
                  tokens_in: Math.max(0, tokensIn),
                  tokens_out: Math.max(0, tokensOut),
                  tokens_cached_in: Math.max(0, tokensCached),
                },
              ],
            }),
          }
        );
        if (!resp.ok) {
          setError(`HTTP ${resp.status}`);
          return;
        }
        const data = (await resp.json()) as PredictResponse;
        setResult(data);
      } catch (e) {
        if ((e as Error).name !== "AbortError") {
          setError("Prediction unavailable");
        }
      }
    }, 150);

    return () => {
      clearTimeout(timer);
      controller.abort();
    };
  }, [provider, tier, windowType, model, tokensIn, tokensOut, tokensCached]);

  return (
    <div className="panel" style={{ marginTop: 0 }}>
      <div className="panel-head">
        <span className="panel-title">Quota Predictor</span>
        <span className="mono" style={{ fontSize: 11, color: "var(--ink-muted)" }}>
          95% pred. interval
        </span>
      </div>
      <div className="panel-body">
        {tier === "all" ? (
          <div className="empty-state">Select a specific tier to run predictions.</div>
        ) : models.length === 0 ? (
          <div className="empty-state">Waiting for model estimates.</div>
        ) : (
          <div className="predictor-form">
            <div className="preset-row">
              <button
                type="button"
                className="btn btn-sm"
                onClick={() => {
                  setTokensIn(5_000);
                  setTokensOut(500);
                  setTokensCached(20_000);
                }}
              >
                Small Turn
              </button>
              <button
                type="button"
                className="btn btn-sm"
                onClick={() => {
                  setTokensIn(20_000);
                  setTokensOut(5_000);
                  setTokensCached(200_000);
                }}
              >
                Coding Loop
              </button>
              <button
                type="button"
                className="btn btn-sm"
                onClick={() => {
                  setTokensIn(50_000);
                  setTokensOut(8_000);
                  setTokensCached(600_000);
                }}
              >
                Heavy Context
              </button>
            </div>
            <div className="field">
              <label>Model</label>
              <select
                className="select"
                value={model}
                onChange={(e) => setModel(e.target.value)}
              >
                {models.map((m) => (
                  <option key={m} value={m}>
                    {m}
                  </option>
                ))}
              </select>
            </div>
            <div className="field">
              <label>Input Tokens</label>
              <input
                type="number"
                className="input"
                min={0}
                step={1000}
                value={tokensIn}
                onChange={(e) => setTokensIn(Number(e.target.value) || 0)}
              />
            </div>
            <div className="field">
              <label>Output Tokens</label>
              <input
                type="number"
                className="input"
                min={0}
                step={500}
                value={tokensOut}
                onChange={(e) => setTokensOut(Number(e.target.value) || 0)}
              />
            </div>
            <div className="field">
              <label>Cached Input Tokens</label>
              <input
                type="number"
                className="input"
                min={0}
                step={10000}
                value={tokensCached}
                onChange={(e) => setTokensCached(Number(e.target.value) || 0)}
              />
            </div>

            {error && (
              <div className="mono" style={{ fontSize: 11, color: "var(--crit)" }}>
                {error}
              </div>
            )}

            {result && (
              <div className="pred-result">
                <div className="kpi-label">Expected Quota Hit ({result.window_type})</div>
                <div className="pred-big-row">
                  <div className="pred-big">{result.expected_delta_pct.toFixed(3)}%</div>
                  {result.expected_delta_pct > 1e-6 && (
                    <span className="mono pred-turns">
                      ~{Math.round(100 / result.expected_delta_pct)} turns / 100% window
                    </span>
                  )}
                </div>
                <div className="mono" style={{ fontSize: 11, color: "var(--ink-muted)" }}>
                  95% CI: [{result.ci_95_lower.toFixed(3)}% – {result.ci_95_upper.toFixed(3)}%] ·{" "}
                  σ̂={result.sigma_hat.toFixed(3)}%
                </div>
              </div>
            )}
          </div>
        )}
      </div>
    </div>
  );
}
