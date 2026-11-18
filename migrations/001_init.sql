CREATE TABLE IF NOT EXISTS intervals (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    idempotency_key TEXT NOT NULL UNIQUE,
    tier TEXT NOT NULL,
    window_type TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER NOT NULL,
    delta_pct REAL NOT NULL,
    quota_precision REAL NOT NULL,
    usage_json TEXT NOT NULL,
    ingested_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_intervals_partition_time
    ON intervals (tier, window_type, ingested_at DESC);

CREATE TABLE IF NOT EXISTS quota_snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    tier TEXT NOT NULL,
    bucket_id TEXT NOT NULL,
    remaining_pct REAL NOT NULL,
    reset_time TEXT,
    recorded_at INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_quota_snapshots_bucket_time
    ON quota_snapshots (bucket_id, recorded_at DESC);
