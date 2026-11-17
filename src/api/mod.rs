pub mod auth;
pub mod estimates;
pub mod telemetry;

use crate::auth::{RegistrationRateTracker, extract_client_ip};
use crate::config::Config;
use crate::db::TelemetryRepository;
use crate::error::AppError;
use crate::worker::SnapshotCache;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use chrono::Utc;
use dashmap::DashMap;
use rust_embed::RustEmbed;
use serde::Serialize;
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tower_http::cors::CorsLayer;
use tower_http::limit::RequestBodyLimitLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    pub repo: Arc<dyn TelemetryRepository>,
    pub cache: SnapshotCache,
    pub config: Arc<Config>,
    pub rate_tracker: Arc<RegistrationRateTracker>,
    pub worker_notify: Arc<Notify>,
    pub http_rate_limiter: Arc<DashMap<IpAddr, VecDeque<i64>>>,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn rate_limit_middleware(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: Request<Body>,
    next: Next,
) -> Result<Response, AppError> {
    let ip = extract_client_ip(req.headers(), peer, state.config.trust_proxy_headers);
    let now = Utc::now().timestamp();
    let max_rpm = state.config.rate_limit_rpm as usize;

    {
        let mut entry = state.http_rate_limiter.entry(ip).or_default();
        while let Some(&front) = entry.front() {
            if now - front >= 60 {
                entry.pop_front();
            } else {
                break;
            }
        }
        if entry.len() >= max_rpm {
            return Err(AppError::RateLimited(
                "too many requests; please slow down".to_string(),
            ));
        }
        entry.push_back(now);
    }

    Ok(next.run(req).await)
}

#[derive(RustEmbed)]
#[folder = "web/dist"]
struct WebAssets;

async fn serve_dashboard(uri: Uri) -> Response {
    let raw_path = uri.path().trim_start_matches('/');
    if raw_path == "v1" || raw_path.starts_with("v1/") {
        return AppError::NotFound("endpoint not found".to_string()).into_response();
    }

    let path = if raw_path.is_empty() {
        "index.html"
    } else {
        raw_path
    };

    if let Some(file) = WebAssets::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        let mut resp = (StatusCode::OK, file.data.into_owned()).into_response();
        if let Ok(val) = HeaderValue::from_str(mime.as_ref()) {
            resp.headers_mut().insert(header::CONTENT_TYPE, val);
        }
        return resp;
    }

    if let Some(index) = WebAssets::get("index.html") {
        let mut resp = (StatusCode::OK, index.data.into_owned()).into_response();
        resp.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/html; charset=utf-8"),
        );
        return resp;
    }

    (StatusCode::NOT_FOUND, "dashboard not built").into_response()
}

pub fn build_router(state: AppState) -> Router {
    let max_body = state.config.max_body_bytes;

    Router::new()
        .route("/health", get(health))
        .route("/v1/auth/challenge", get(auth::get_challenge))
        .route("/v1/auth/register", post(auth::register))
        .route("/v1/telemetry/intervals", post(telemetry::ingest_intervals))
        .route("/v1/estimates", get(estimates::list_partitions))
        .route("/v1/estimates/{provider}/{tier}", get(estimates::get_estimates))
        .route(
            "/v1/estimates/{provider}/{tier}/history",
            get(estimates::get_estimates_history),
        )
        .route(
            "/v1/estimates/{provider}/{tier}/predict",
            post(estimates::predict_quota),
        )
        .fallback(serve_dashboard)
        .layer(middleware::from_fn_with_state(
            state.clone(),
            rate_limit_middleware,
        ))
        .layer(RequestBodyLimitLayer::new(max_body))
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(15),
        ))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
