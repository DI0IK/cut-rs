# `cut-rs`

A production-grade Rust backend service and local telemetry client that ingests crowdsourced AI quota consumption across providers and subscription tiers, estimates per-token quota costs (`tokens_in`, `tokens_out`, `tokens_cached_in`) in real time, resists Sybil poisoning, and detects silent provider pricing/quota shifts.

Default public server & live dashboard: **`https://cut-rs.dominikstahl.dev`**

---

## 30-Second Quickstart

**1. Install the binary:**
```bash
# Linux & macOS
curl -fsSL https://raw.githubusercontent.com/DI0IK/cut-rs/main/scripts/install.sh | sh
```
```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/DI0IK/cut-rs/main/scripts/install.ps1 | iex
```
*(Or download a prebuilt archive from [GitHub Releases](https://github.com/DI0IK/cut-rs/releases).)*

**2. Auto-configure your AI coding tools & verify:**
```bash
cut-rs init
cut-rs status
```
`cut-rs init` detects **Claude Code**, **OpenAI Codex**, **Antigravity CLI**, and **Waybar**, backs up existing configs (`.bak`), and **prepends** `cut-rs --chain ...` if you already have a custom `statusLine` so your existing status line keeps working.

> **Privacy & ToS:** Only token count deltas and quota `%` changes are sent. Prompts, code, and raw IP addresses are never collected or stored. All adapters use official local interfaces (`statusLine`, `notify`, local rollout logs).

---

## Dashboard

`cut-rs` serves an embedded React + TypeScript + Vite dashboard at `/` (bundled into the single binary via `rust-embed`):
- **Quota Cost Timeline:** Compares models across sub-tiers and quota buckets over time using **Blended Mix** (weighted by the window's empirical crowdsourced `in / out / cached` token shares) or individual `OUT` / `IN` / `CACHED` breakdowns, with regime-shift markers.
- **Active Model Weights & Predictor:** Displays `pct_per_1m_*` estimates with 95% confidence intervals, identifiability ratings, empirical cache rate, and an interactive Quota Predictor.
- **Change Log:** Surfaces `regime_shift`, `model_added`, `model_retired`, `bucket_added`, and `bucket_retired` events.

---

## Architecture & Statistical Pipeline

All state and estimation are partitioned independently by `(provider, tier, window_type)` and discovered dynamically from incoming telemetry—supporting providers adding/retiring quota buckets or moving models between buckets without configuration changes.

- **Web & Concurrency:** `axum` + `tower-http` (tracing, body size limits, timeouts, per-IP rate limiting) with a lock-free `DashMap` snapshot cache hydrated from SQLite (`WAL` mode, `synchronous = NORMAL`) and updated by a background Tokio worker.
- **Solver (IRLS + Active-Set NNLS):**
  - Lawson-Hanson Active-Set Non-Negative Least Squares (`nalgebra`).
  - Sample weights combine 14-day exponential time-decay, inverse quantization variance, client `trust_score`, and a **5% per-client contribution cap**.
  - **Asymmetric Huber clipping** ($\delta_+ = 0.8 \cdot \text{MAD}$ for positive residuals vs $\delta_- = 1.345 \cdot \text{MAD}$) down-weights unlogged usage from other devices/apps.
  - Computes covariance $\Sigma_{\mathbf{w}}$, Student's $t$ 95% confidence intervals (`statrs`), SVD condition-number identifiability (`high`, `moderate`, `low_collinear`), and 95% prediction intervals.
- **Change-Point Detection:** Two-sided CUSUM + dual-window F-test ($W_{\text{long}} = 1500$ vs $W_{\text{short}} = 60$), gated by an **anti-poisoning consensus rule** requiring $\ge 5$ distinct `trusted` clients across $\ge 3$ distinct `/24` IPv4 (`/48` IPv6) `subnet_hash` buckets.
- **Anti-Sybil Auth:** Stateless HMAC-signed SHA-256 Proof-of-Work challenges (`GET /v1/auth/challenge`) with dynamic difficulty scaling, daily-salted subnet hashing (raw IPs are never persisted), and `probation` $\rightarrow$ `trusted` promotion.

---

## HTTP API

| Method | Path | Auth | Description |
| :--- | :--- | :--- | :--- |
| `GET` | `/health` | Public | Health check |
| `GET` | `/v1/auth/challenge` | Public | Issue PoW challenge (`difficulty_bits`, 120s TTL) |
| `POST` | `/v1/auth/register` | Public | Verify PoW and issue `qt_<uuid>.<sig>` bearer token |
| `POST` | `/v1/telemetry/intervals` | Bearer | Ingest interval batches (partial-success status per item) |
| `GET` | `/v1/estimates` | Public | List all discovered `(provider, tier, window_type)` partitions & empirical token mix |
| `GET` | `/v1/estimates/{provider}/{tier}` | Public | Active model weights, 95% CIs, identifiability, change events (`?window_type=`, `?include_stale=`) |
| `GET` | `/v1/estimates/{provider}/{tier}/history` | Public | Historical snapshot timeline (`tier=all` supported, `?window_type=`, `?limit=`) |
| `POST` | `/v1/estimates/{provider}/{tier}/predict` | Public | Predict quota % hit and 95% prediction interval |

---

## Client & Desktop Integrations (ToS-Compliant)

All client adapters use **only official, local interfaces** (no cookie extraction, no token spoofing, no private API scraping). By default, all client subcommands target `https://cut-rs.dominikstahl.dev` (override with `--server <URL>` or `CUT_RS_SERVER`) and share a single client registration:
- **Linux:** `~/.local/state/cut-rs/auth.json`
- **macOS:** `~/Library/Application Support/cut-rs/auth.json`
- **Windows:** `%LOCALAPPDATA%\cut-rs\auth.json`

### Install Prebuilt Binary (Linux / macOS / Windows)
Download the latest release archive for your platform (`x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, `x86_64-apple-darwin`, or `x86_64-pc-windows-msvc`) from [GitHub Releases](https://github.com/DI0IK/cut-rs/releases), extract `cut-rs` (or `cut-rs.exe`), and place it on your `PATH`.

### 1. Antigravity CLI (`cut-rs agy-cli`)
Add to `~/.gemini/antigravity-cli/settings.json` (Windows: `%USERPROFILE%\.gemini\antigravity-cli\settings.json`):
```json
{
  "statusLine": {
    "type": "command",
    "command": "cut-rs agy-cli",
    "stack_with_default": true
  }
}
```

### 2. Claude Code CLI & IDE Extension (`cut-rs claude-cli`)
Works with **Claude Code CLI** and **Claude Code IDE extensions** (VS Code / JetBrains / Claude Desktop Code tab) via `~/.claude/settings.json` (Windows: `%USERPROFILE%\.claude\settings.json`):
```json
{
  "statusLine": {
    "type": "command",
    "command": "cut-rs claude-cli --tier pro"
  }
}
```
*(Set `--tier` or `CUT_RS_TIER` to `pro`, `max_5x`, `max_20x`, etc.)*

### 3. OpenAI Codex CLI, Desktop App, & VS Code Extension (`cut-rs codex-cli`)
Reads local session rollout logs (`~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl`, Windows: `%USERPROFILE%\.codex\sessions\...`) shared by **Codex CLI**, **Codex Desktop**, and the **Codex VS Code extension**:
- **Codex `notify` hook** (`~/.codex/config.toml`):
  ```toml
  notify = ["cut-rs", "codex-cli"]
  ```
- **Continuous watcher** (e.g. for Codex Desktop / VS Code):
  ```bash
  cut-rs codex-cli --watch
  ```

### 4. Waybar Module (`cut-rs waybar`)
Aggregates local quota state across all active tools and outputs Waybar JSON (`<5h>% / <weekly>%` + hover tooltip with reset countdowns):
```jsonc
"custom/cut-rs": {
  "exec": "/path/to/cut-rs waybar --server https://cut-rs.dominikstahl.dev",
  "return-type": "json",
  "interval": 5,
  "format": "󰚩 {}",
  "tooltip": true
}
```
Options: `--provider <name>`, `--show-used`, `--server <url>`.

---

## Building & Deploying

### Local Build & Tests
```bash
# Build embedded dashboard assets (required once before cargo build/test)
devenv shell -- bash -c "cd web && npm install && npm run build"

devenv shell -- cargo build --release
devenv shell -- cargo test
devenv shell -- cargo clippy --all-targets -- -D warnings

# Run the HTTP server
./target/release/cut-rs serve
```

### NixOS Flake Deployment
`flake.nix` provides `packages.<system>.default` and `nixosModules.default` (`services.cut-rs` with a hardened `systemd` unit):
```nix
{
  inputs.cut-rs.url = "github:DI0IK/cut-rs";

  outputs = { nixpkgs, cut-rs, ... }: {
    nixosConfigurations.server = nixpkgs.lib.nixosSystem {
      system = "x86_64-linux";
      modules = [
        cut-rs.nixosModules.default
        {
          services.cut-rs = {
            enable = true;
            host = "127.0.0.1";
            port = 8080;
            trustProxyHeaders = true;
            environmentFile = "/run/secrets/cut-rs-env"; # HMAC_SECRET=...
          };
        }
      ];
    };
  };
}
```

### Server Environment Variables
| Variable | Default | Description |
| :--- | :--- | :--- |
| `HOST` | `0.0.0.0` | Bind address |
| `PORT` | `8080` | Bind port |
| `DATABASE_URL` | `sqlite://cut_rs.db?mode=rwc` | SQLite database URL |
| `HMAC_SECRET` | *(ephemeral random)* | Secret key for PoW challenges, bearer tokens, and daily subnet salts |
| `TRUST_PROXY_HEADERS` | `false` | Trust `X-Forwarded-For` / `X-Real-IP` behind a reverse proxy |
| `RATE_LIMIT_RPM` | `120` | Per-IP requests per minute |
| `WORKER_INTERVAL_SECS` | `10` | Background estimation loop interval |
