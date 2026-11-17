use cut_rs::api::build_router;
use cut_rs::auth::solve_pow_for_test;
use cut_rs::config::Config;
use cut_rs::init_app_state;
use cut_rs::worker::process_all_partitions;
use serde_json::Value;
use std::net::SocketAddr;
use tempfile::TempDir;

#[tokio::test]
async fn test_end_to_end_auth_ingest_estimate_predict_and_bucket_migration() {
    let dir = TempDir::new().unwrap();
    let db_path = dir.path().join("test.db");
    let db_path_str = db_path.to_string_lossy().replace('\\', "/");
    let db_url = format!("sqlite://{db_path_str}?mode=rwc");

    let config = Config::test_default(db_url);
    let state = init_app_state(config).await.unwrap();
    let worker_state = state.clone();

    let router = build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let server_handle = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });

    let base = format!("http://{addr}");
    let client = reqwest::Client::new();

    // 1. Health check
    let health = client.get(format!("{base}/health")).send().await.unwrap();
    assert_eq!(health.status(), 200);

    // 2. Register 3 clients via PoW challenge
    let mut tokens = Vec::new();
    for _ in 0..3 {
        let ch: Value = client
            .get(format!("{base}/v1/auth/challenge"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let challenge = ch["challenge"].as_str().unwrap().to_string();
        let diff = ch["difficulty_bits"].as_u64().unwrap() as u8;
        let nonce = solve_pow_for_test(&challenge, diff);

        let reg: Value = client
            .post(format!("{base}/v1/auth/register"))
            .json(&serde_json::json!({ "challenge": challenge, "nonce": nonce }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();

        assert_eq!(reg["trust_tier"], "probation");
        assert_eq!(reg["trust_score"], 0.1);
        tokens.push(reg["token"].as_str().unwrap().to_string());
    }

    // 3. Submit batch with valid, duplicate, and invalid intervals
    // True costs in 5h bucket: gemini-4 -> in=2.0, out=10.0, cached=0.2 pct/1M
    let now = chrono::Utc::now().timestamp();
    let mut valid_batch = Vec::new();
    for i in 0..15 {
        let t_in = 100_000 + i * 25_000;
        let t_out = 10_000 + i * 5_000;
        let t_cache = 40_000 + ((i * 7) % 11) * 15_000;
        let delta = 2.0 * (t_in as f64 / 1e6)
            + 10.0 * (t_out as f64 / 1e6)
            + 0.2 * (t_cache as f64 / 1e6);

        valid_batch.push(serde_json::json!({
            "idempotency_key": format!("valid-{i}"),
            "provider": "antigravity",
            "tier": "pro",
            "window_type": "5h",
            "started_at": now - 3600 + (i as i64) * 100,
            "ended_at": now - 3540 + (i as i64) * 100,
            "quota_before_pct": 90.0,
            "quota_after_pct": 90.0 - delta,
            "quota_precision": 0.01,
            "usage": [{
                "model": "gemini-4",
                "tokens_in": t_in,
                "tokens_out": t_out,
                "tokens_cached_in": t_cache,
                "request_count": 1
            }]
        }));
    }
    // Add a duplicate of valid-0 and an invalid negative delta
    valid_batch.push(valid_batch[0].clone());
    valid_batch.push(serde_json::json!({
        "idempotency_key": "bad-delta",
        "provider": "antigravity",
        "tier": "pro",
        "window_type": "5h",
        "started_at": now - 3600,
        "ended_at": now - 3540,
        "quota_before_pct": 80.0,
        "quota_after_pct": 85.0,
        "usage": [{ "model": "gemini-4", "tokens_in": 1000, "tokens_out": 100 }]
    }));

    let ingest_resp: Value = client
        .post(format!("{base}/v1/telemetry/intervals"))
        .bearer_auth(&tokens[0])
        .json(&serde_json::json!({ "intervals": valid_batch }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(ingest_resp["accepted"], 15);
    assert_eq!(ingest_resp["duplicate"], 1);
    assert_eq!(ingest_resp["rejected"], 1);

    // 4. Add a second bucket dynamically (`flash-daily`) with `gemini-4-flash`
    let mut flash_batch = Vec::new();
    for i in 0..6 {
        let t_in = 200_000 + i * 50_000;
        let t_out = 20_000 + i * 10_000;
        let delta = 0.5 * (t_in as f64 / 1e6) + 2.5 * (t_out as f64 / 1e6);
        flash_batch.push(serde_json::json!({
            "idempotency_key": format!("flash-{i}"),
            "provider": "antigravity",
            "tier": "pro",
            "window_type": "flash-daily",
            "started_at": now - 1000 + (i as i64) * 100,
            "ended_at": now - 940 + (i as i64) * 100,
            "quota_before_pct": 100.0,
            "quota_after_pct": 100.0 - delta,
            "usage": [{
                "model": "gemini-4-flash",
                "tokens_in": t_in,
                "tokens_out": t_out,
                "tokens_cached_in": 0
            }]
        }));
    }
    let flash_resp = client
        .post(format!("{base}/v1/telemetry/intervals"))
        .bearer_auth(&tokens[1])
        .json(&serde_json::json!({ "intervals": flash_batch }))
        .send()
        .await
        .unwrap();
    assert_eq!(flash_resp.status(), 200);

    // 5. Run worker estimation pass
    process_all_partitions(
        worker_state.repo.as_ref(),
        &worker_state.cache,
        &worker_state.config,
    )
    .await
    .unwrap();

    // 6. Query GET /v1/estimates/antigravity/pro
    let est: Value = client
        .get(format!("{base}/v1/estimates/antigravity/pro"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let windows = est["windows"].as_array().unwrap();
    assert_eq!(windows.len(), 2);
    assert_eq!(windows[0]["window_type"], "5h");
    assert_eq!(windows[1]["window_type"], "flash-daily");

    let m5h = &windows[0]["models"][0];
    assert_eq!(m5h["model"], "gemini-4");
    assert!((m5h["pct_per_1m_in"].as_f64().unwrap() - 2.0).abs() < 0.2);
    assert!((m5h["pct_per_1m_out"].as_f64().unwrap() - 10.0).abs() < 0.5);

    // 7. Query POST /v1/estimates/antigravity/pro/predict
    let pred: Value = client
        .post(format!("{base}/v1/estimates/antigravity/pro/predict"))
        .json(&serde_json::json!({
            "window_type": "5h",
            "usage": [{
                "model": "gemini-4",
                "tokens_in": 500_000,
                "tokens_out": 100_000,
                "tokens_cached_in": 0
            }]
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    // Expected: 0.5 * 2.0 + 0.1 * 10.0 = 2.0%
    let expected = pred["expected_delta_pct"].as_f64().unwrap();
    assert!((expected - 2.0).abs() < 0.2, "expected={expected}");
    assert!(pred["ci_95_lower"].as_f64().unwrap() <= expected);
    assert!(pred["ci_95_upper"].as_f64().unwrap() >= expected);

    // 8. Query GET /v1/estimates (partition index), /history, and GET / (dashboard HTML)
    let index: Value = client
        .get(format!("{base}/v1/estimates"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(index["partitions"].as_array().unwrap().len(), 2);

    let hist: Value = client
        .get(format!("{base}/v1/estimates/antigravity/all/history"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let hist_points = hist["points"].as_array().unwrap();
    assert_eq!(hist_points.len(), 21);
    assert!(!hist_points.last().unwrap()["models"].as_array().unwrap().is_empty());

    let html_resp = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(html_resp.status(), 200);
    let html = html_resp.text().await.unwrap();
    assert!(html.contains("<div id=\"root\"></div>"));

    server_handle.abort();
}
