pub mod estimates;

use crate::config::Config;
use crate::db::TelemetryRepository;
use crate::error::AppError;
use axum::Json;
use axum::Router;
use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use rust_embed::RustEmbed;
use serde::Serialize;
use std::sync::Arc;
use std::time::Duration;
use tower_http::cors::CorsLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;

#[derive(Clone)]
pub struct AppState {
    pub repo: Arc<dyn TelemetryRepository>,
    pub config: Arc<Config>,
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
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
    Router::new()
        .route("/health", get(health))
        .route("/v1/quota", get(estimates::get_quota_status))
        .route("/v1/estimates", get(estimates::list_partitions))
        .route(
            "/v1/estimates/{provider}/{tier}",
            get(estimates::get_estimates),
        )
        .route(
            "/v1/estimates/{provider}/{tier}/history",
            get(estimates::get_estimates_history),
        )
        .route(
            "/v1/estimates/{provider}/{tier}/predict",
            post(estimates::predict_quota),
        )
        .fallback(serve_dashboard)
        .layer(TimeoutLayer::with_status_code(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(15),
        ))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
