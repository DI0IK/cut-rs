# `cut-rs`

A local-only, single-user Antigravity CLI quota tracker, Waybar module, and on-demand token cost estimator.

- **Zero server required for tracking**: `cut-rs agy-cli` runs inline from Antigravity's `statusLine` hook, tails local transcripts, locks bucket expiry timestamps once quota drops below 100%, and writes `token -> % drop` intervals and historical quota snapshots to a local SQLite database (`~/.local/state/cut-rs/cut-rs.db`).
- **On-demand statistical estimation**: Model token costs (`tokens_in`, `tokens_out`, `tokens_cached_in`) and historical timelines are computed on demand from SQLite—no background estimation daemon or auth tokens.
- **Compact Waybar integration**: Displays remaining/used quota percentages in the bar and a clean hover tooltip with bucket reset countdowns.
- **On-demand web dashboard**: Start or stop the local web UI whenever you want with `cut-rs webserver start --open` / `cut-rs webserver stop`.

---

## Quickstart

### 1. Build or Install
```bash
# Using Cargo
cargo install --path .

# Or using Nix
nix build
```

### 2. Configure Antigravity CLI & Waybar
```bash
cut-rs init
cut-rs status
```
`cut-rs init` updates `~/.gemini/antigravity-cli/settings.json` with `cut-rs agy-cli` (chaining any preexisting `statusLine` command via `--chain`) and prints the Waybar custom module snippet.

### 3. Waybar Module (`cut-rs waybar`)
Add to `~/.config/waybar/config.jsonc`:
```jsonc
"custom/cut-rs": {
  "exec": "cut-rs waybar",
  "return-type": "json",
  "interval": 15,
  "format": "⚡ {text}"
}
```
Pass `--show-used` if you prefer used `%` instead of remaining `%`.

### 4. On-Demand Web Dashboard (`cut-rs webserver`)
```bash
cut-rs webserver start --open   # Starts background server on http://127.0.0.1:8080 and opens browser
cut-rs webserver status         # Check PID & URL
cut-rs webserver stop           # Stop background server
```
Use `--foreground` or `--port <PORT>` on `start` if needed.

---

## Dashboard & HTTP API

The embedded React + TypeScript + Vite dashboard (`web/dist`, bundled via `rust-embed`) provides:
- **Current Quota Status**: Live remaining `%`, progress bars, and locked reset countdowns from `agy-cli-state.json`, plus historical quota snapshot count from SQLite.
- **Quota Cost Timeline**: On-demand replay of per-model `pct_per_1m_*` estimates over time with cost-shift markers.
- **Active Model Weights & Predictor**: Model cost table with 95% confidence intervals, identifiability ratings, and an interactive quota-hit predictor.

| Method | Path | Description |
| :--- | :--- | :--- |
| `GET` | `/health` | Health check |
| `GET` | `/v1/quota` | Current quota buckets (`agy-cli-state.json`) & `quota_snapshots` history |
| `GET` | `/v1/estimates` | Active `(tier, window_type)` partitions & empirical token mix |
| `GET` | `/v1/estimates/{provider}/{tier}` | On-demand model weights, 95% CIs, identifiability, change events |
| `GET` | `/v1/estimates/{provider}/{tier}/history` | On-demand historical timeline (`tier=all` supported) |
| `POST` | `/v1/estimates/{provider}/{tier}/predict` | Predict quota `%` hit and 95% prediction interval |

---

## Statistical Pipeline

All cost estimation runs on demand from the `intervals` table in SQLite:
- **IRLS + Active-Set NNLS**: Lawson-Hanson non-negative least squares (`nalgebra`) with monotonic constraints (`w_out >= w_in >= w_cached >= 0`), 14-day exponential time-decay, and inverse quantization variance weighting.
- **Outlier Resistance**: High-breakdown LTS seed + asymmetric Huber/Tukey clipping downweight positive spikes from unlogged usage (e.g. concurrent Antigravity IDE sessions).
- **Change-Point Detection**: Two-sided CUSUM + dual-window F-test detects token-cost shifts and slices the active regime automatically.
