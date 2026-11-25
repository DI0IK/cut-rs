export interface TokenMix {
  in_share: number;
  out_share: number;
  cached_share: number;
}

export interface ModelWeightEstimate {
  model: string;
  priced: boolean;
  usd_per_1m_in: number;
  usd_per_1m_out: number;
  usd_per_1m_cached_in: number;
  pct_per_1m_in: number;
  pct_per_1m_out: number;
  pct_per_1m_cached_in: number;
  total_tokens_in: number;
  total_tokens_out: number;
  total_tokens_cached_in: number;
  total_requests: number;
  total_api_usd: number;
  sample_count: number;
  last_seen_at: number;
  status: "active" | "retired";
}

export interface PartitionSummary {
  provider: string;
  tier: string;
  window_type: string;
  sample_count_window: number;
  bucket_api_equivalent_usd: number;
  pct_per_usd: number;
  token_mix: TokenMix;
  stale: boolean;
  computed_at: number;
}

export interface WindowEstimateView {
  window_type: string;
  computed_at: number;
  sample_count_window: number;
  stale: boolean;
  pct_per_usd: number;
  bucket_api_equivalent_usd: number;
  pct_per_usd_p25: number;
  pct_per_usd_p75: number;
  total_api_usd: number;
  total_delta_pct: number;
  token_mix: TokenMix;
  models: ModelWeightEstimate[];
}

export interface HistorySnapshotPoint {
  tier: string;
  window_type: string;
  computed_at: number;
  sample_count_window: number;
  pct_per_usd: number;
  bucket_api_equivalent_usd: number;
  token_mix: TokenMix;
}

export interface PredictResponse {
  provider: string;
  tier: string;
  window_type: string;
  expected_delta_pct: number;
  expected_api_usd: number;
  ci_95_lower: number;
  ci_95_upper: number;
  pct_per_usd: number;
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

export interface QuotaChangeEvent {
  id: number;
  tier: string;
  bucket_id: string;
  recorded_at: number;
  prev_pct: number | null;
  curr_pct: number;
  delta_pct: number | null;
  reset_time: string | null;
  event_type: "init" | "reset" | "decrease" | "anomaly_increase";
}

export interface QuotaStatusResponse {
  provider: string;
  tier: string | null;
  buckets: CurrentBucketStatus[];
  history: QuotaSnapshotRow[];
  changes: QuotaChangeEvent[];
}
