CREATE TABLE IF NOT EXISTS clients (
    client_id TEXT PRIMARY KEY,
    created_at INTEGER NOT NULL,
    subnet_hash TEXT NOT NULL,
    trust_score REAL NOT NULL DEFAULT 0.10,
    trust_tier TEXT NOT NULL DEFAULT 'probation',
    accepted_count INTEGER NOT NULL DEFAULT 0,
    ema_abs_z REAL NOT NULL DEFAULT 0.0,
    last_seen_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_clients_subnet_created
    ON clients (subnet_hash, created_at DESC);

CREATE TABLE IF NOT EXISTS intervals (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    idempotency_key TEXT NOT NULL UNIQUE,
    client_id TEXT NOT NULL REFERENCES clients(client_id),
    provider TEXT NOT NULL,
    tier TEXT NOT NULL,
    window_type TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER NOT NULL,
    delta_pct REAL NOT NULL,
    quota_precision REAL NOT NULL,
    usage_json TEXT NOT NULL,
    residual_pct REAL,
    weight_assigned REAL,
    ingested_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_intervals_partition_time
    ON intervals (provider, tier, window_type, ingested_at DESC);

CREATE INDEX IF NOT EXISTS idx_intervals_client_time
    ON intervals (client_id, ingested_at DESC);

CREATE TABLE IF NOT EXISTS model_snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    provider TEXT NOT NULL,
    tier TEXT NOT NULL,
    window_type TEXT NOT NULL,
    computed_at INTEGER NOT NULL,
    regime_since INTEGER NOT NULL,
    sample_count INTEGER NOT NULL,
    effective_contributors REAL NOT NULL,
    weights_json TEXT NOT NULL,
    cusum_pos REAL NOT NULL DEFAULT 0.0,
    cusum_neg REAL NOT NULL DEFAULT 0.0
);

CREATE INDEX IF NOT EXISTS idx_snapshots_partition_time
    ON model_snapshots (provider, tier, window_type, computed_at DESC);
