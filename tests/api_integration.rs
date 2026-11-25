use axum::body::Body;
use axum::http::{Request, StatusCode};
use cut_rs::api::build_router;
use cut_rs::cli::common::{BucketTracker, CliToolState, save_state};
use cut_rs::config::Config;
use cut_rs::db::NewInterval;
use cut_rs::init_app_state;
use cut_rs::models::UsageEntry;
use http_body_util::BodyExt;
use serde_json::Value;
use std::collections::HashMap;
use tempfile::TempDir;
use tower::ServiceExt;

async fn get_json(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let val = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, val)
}

async fn post_json(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let val = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, val)
}

#[tokio::test]
async fn test_local_sqlite_on_demand_estimates_history_predict_and_quota() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let db_path_str = db_path.to_string_lossy().replace('\\', "/");
    let db_url = format!("sqlite://{db_path_str}?mode=rwc");
    let state_path = dir.path().join("agy-cli-state.json");

    let config = Config::test_default(db_url, state_path.clone());
    let state = init_app_state(config).await.unwrap();

    // Populate state file with a locked bucket expiry
    let mut buckets = HashMap::new();
    buckets.insert(
        "5h".to_string(),
        BucketTracker {
            last_remaining_fraction: 0.85,
            last_reset_time: Some("2026-11-18T14:00:00Z".to_string()),
            locked_reset_time: Some("2026-11-18T13:55:00Z".to_string()),
            started_at: 1_700_000_000,
            pending_usage: HashMap::new(),
        },
    );
    save_state(
        &state_path,
        &CliToolState {
            provider: Some("antigravity".to_string()),
            tier: Some("pro".to_string()),
            buckets,
            ..Default::default()
        },
    )
    .unwrap();

    let now = chrono::Utc::now().timestamp();
    state
        .repo
        .insert_quota_snapshot("pro", "5h", 85.0, Some("2026-11-18T13:55:00Z"), now)
        .await
        .unwrap();

    // Insert 15 synthetic intervals for hydrogen ($1.5 / $7.5 / $0.075 per 1M),
    // with 1 USD = 1.0% quota (so 100% bucket = $100 API equivalent, in=1.5%, out=7.5%, cached=0.075%).
    for i in 0..15 {
        let t_in = 100_000 + i * 25_000;
        let t_out = 10_000 + i * 5_000;
        let t_cache = 40_000 + ((i * 7) % 11) * 15_000;
        let delta = 1.5 * (t_in as f64 / 1e6)
            + 7.5 * (t_out as f64 / 1e6)
            + 0.075 * (t_cache as f64 / 1e6);

        let inserted = state
            .repo
            .insert_interval_if_new(&NewInterval {
                idempotency_key: format!("valid-{i}"),
                tier: "pro".to_string(),
                window_type: "5h".to_string(),
                started_at: now - 3600 + (i as i64) * 100,
                ended_at: now - 3540 + (i as i64) * 100,
                delta_pct: delta,
                quota_precision: 0.01,
                usage: vec![UsageEntry {
                    model: "gemini 4 hydrogen (medium)".to_string(),
                    tokens_in: t_in,
                    tokens_out: t_out,
                    tokens_cached_in: t_cache,
                    request_count: 1,
                }],
                ingested_at: now - 3540 + (i as i64) * 100,
            })
            .await
            .unwrap();
        assert!(inserted);
    }

    let app = build_router(state);

    let (st, health) = get_json(&app, "/health").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(health["status"], "ok");

    let (st, quota) = get_json(&app, "/v1/quota").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(quota["tier"], "pro");
    assert_eq!(quota["buckets"][0]["bucket_id"], "5h");
    assert_eq!(quota["buckets"][0]["reset_time"], "2026-11-18T13:55:00Z");
    assert_eq!(quota["history"].as_array().unwrap().len(), 1);
    assert_eq!(quota["changes"].as_array().unwrap().len(), 1);

    let (st, parts) = get_json(&app, "/v1/estimates").await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(parts["partitions"].as_array().unwrap().len(), 1);

    let (st, est) = get_json(&app, "/v1/estimates/antigravity/pro").await;
    assert_eq!(st, StatusCode::OK);
    let win = &est["windows"][0];
    assert!((win["bucket_api_equivalent_usd"].as_f64().unwrap() - 100.0).abs() < 1e-4);
    let m = &win["models"][0];
    assert_eq!(m["model"], "gemini 4 hydrogen (medium)");
    assert!((m["pct_per_1m_in"].as_f64().unwrap() - 1.5).abs() < 1e-4);
    assert!((m["pct_per_1m_out"].as_f64().unwrap() - 7.5).abs() < 1e-4);

    let (st, hist) = get_json(&app, "/v1/estimates/antigravity/pro/history").await;
    assert_eq!(st, StatusCode::OK);
    assert!(!hist["points"].as_array().unwrap().is_empty());

    let (st, pred) = post_json(
        &app,
        "/v1/estimates/antigravity/pro/predict",
        serde_json::json!({
            "window_type": "5h",
            "usage": [{
                "model": "gemini 4 hydrogen (medium)",
                "tokens_in": 1_000_000,
                "tokens_out": 100_000,
                "tokens_cached_in": 500_000,
                "request_count": 1
            }]
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let expected = pred["expected_delta_pct"].as_f64().unwrap();
    // 1.0*1.5 + 0.1*7.5 + 0.5*0.075 = 2.2875
    assert!((expected - 2.2875).abs() < 1e-4, "expected={expected}");
    assert!((pred["expected_api_usd"].as_f64().unwrap() - 2.2875).abs() < 1e-4);
}
