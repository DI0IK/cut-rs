export interface TokenMix {
  in_share: number;
  out_share: number;
  cached_share: number;
}

export interface ConfidenceInterval {
  lower: number;
  upper: number;
}

export interface ModelWeightEstimate {
  model: string;
  pct_per_1m_in: number;
  pct_per_1m_in_ci: ConfidenceInterval;
  pct_per_1m_out: number;
  pct_per_1m_out_ci: ConfidenceInterval;
  pct_per_1m_cached_in: number;
  pct_per_1m_cached_in_ci: ConfidenceInterval;
  identifiability: "high" | "moderate" | "low_collinear" | "insufficient_samples";
  sample_count: number;
  last_seen_at: number;
  status: "active" | "retired";
  token_mix?: TokenMix;
}

export interface ChangeEvent {
  timestamp: number;
  event_type:
    | "cost_change"
    | "model_added"
    | "model_retired"
    | "bucket_added"
    | "bucket_retired";
  description: string;
  cusum_value?: number;
  p_value?: number;
}

export interface PartitionSummary {
  provider: string;
  tier: string;
  window_type: string;
  sample_count_window: number;
  token_mix: TokenMix;
  stale: boolean;
  computed_at: number;
}

export interface WindowEstimateView {
  window_type: string;
  computed_at: number;
  regime_since: number;
  sample_count_window: number;
  sigma_hat: number;
  stale: boolean;
  token_mix: TokenMix;
  models: ModelWeightEstimate[];
  recent_change_events: ChangeEvent[];
}

export interface HistoryModelPoint {
  model: string;
  pct_per_1m_in: number;
  pct_per_1m_out: number;
  pct_per_1m_cached_in: number;
}

export interface HistorySnapshotPoint {
  tier: string;
  window_type: string;
  computed_at: number;
  regime_since: number;
  sample_count_window: number;
  token_mix: TokenMix;
  models: HistoryModelPoint[];
  recent_change_events: ChangeEvent[];
}

export interface PredictResponse {
  provider: string;
  tier: string;
  window_type: string;
  expected_delta_pct: number;
  ci_95_lower: number;
  ci_95_upper: number;
  sigma_hat: number;
}

export interface CurrentBucketStatus {
  bucket_id: string;
  remaining_pct: number;
  reset_time: string | null;
  started_at: number;
}

export interface QuotaSnapshotRow {
  id: number;
  tier: string;
  bucket_id: string;
  remaining_pct: number;
  reset_time: string | null;
  recorded_at: number;
}

export interface QuotaStatusResponse {
  provider: string;
  tier: string | null;
  buckets: CurrentBucketStatus[];
  history: QuotaSnapshotRow[];
}
