use crate::api::AppState;
use crate::auth::{
    extract_client_ip, hash_subnet, issue_bearer_token, issue_challenge,
    verify_bearer_token, verify_challenge_and_pow,
};
use crate::error::AppError;
use crate::models::{ClientRecord, TrustTier};
use axum::Json;
use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::HeaderMap;
use axum::http::request::Parts;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use uuid::Uuid;

#[derive(Debug, Serialize)]
pub struct ChallengeResponse {
    pub challenge: String,
    pub difficulty_bits: u8,
    pub expires_at: i64,
}

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    pub challenge: String,
    pub nonce: String,
}

#[derive(Debug, Serialize)]
pub struct RegisterResponse {
    pub client_id: String,
    pub token: String,
    pub trust_tier: TrustTier,
    pub trust_score: f64,
}

pub async fn get_challenge(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
) -> Result<Json<ChallengeResponse>, AppError> {
    let now_dt = Utc::now();
    let now = now_dt.timestamp();
    let ip = extract_client_ip(&headers, peer, state.config.trust_proxy_headers);
    let subnet_hash = hash_subnet(ip, &state.config.hmac_secret, now_dt);

    let difficulty_bits = state.rate_tracker.difficulty_bits(
        &subnet_hash,
        state.config.pow_base_difficulty,
        state.config.pow_max_difficulty,
        now,
    );
    let challenge = issue_challenge(&state.config.hmac_secret, difficulty_bits, now);

    Ok(Json(ChallengeResponse {
        challenge,
        difficulty_bits,
        expires_at: now + state.config.pow_ttl_secs,
    }))
}

pub async fn register(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(req): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>, AppError> {
    let now_dt = Utc::now();
    let now = now_dt.timestamp();

    verify_challenge_and_pow(
        &req.challenge,
        &req.nonce,
        &state.config.hmac_secret,
        state.config.pow_ttl_secs,
        now,
    )?;

    let ip = extract_client_ip(&headers, peer, state.config.trust_proxy_headers);
    let subnet_hash = hash_subnet(ip, &state.config.hmac_secret, now_dt);
    state.rate_tracker.record_registration(&subnet_hash, now);

    let client_id = Uuid::new_v4().to_string();
    let client = ClientRecord {
        client_id: client_id.clone(),
        created_at: now,
        subnet_hash,
        trust_score: 0.10,
        trust_tier: TrustTier::Probation,
        accepted_count: 0,
        ema_abs_z: 0.0,
        last_seen_at: now,
    };

    state.repo.insert_client(&client).await?;
    let token = issue_bearer_token(&client_id, &state.config.hmac_secret);

    Ok(Json(RegisterResponse {
        client_id,
        token,
        trust_tier: TrustTier::Probation,
        trust_score: 0.10,
    }))
}

pub struct AuthenticatedClient(pub ClientRecord);

impl FromRequestParts<AppState> for AuthenticatedClient {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let auth_header = parts
            .headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or_else(|| AppError::Unauthorized("missing Authorization header".to_string()))?;

        let token = auth_header
            .strip_prefix("Bearer ")
            .or_else(|| auth_header.strip_prefix("bearer "))
            .ok_or_else(|| AppError::Unauthorized("invalid Authorization scheme".to_string()))?;

        let client_id = verify_bearer_token(token.trim(), &state.config.hmac_secret)
            .ok_or_else(|| AppError::Unauthorized("invalid bearer token".to_string()))?;

        let client = state
            .repo
            .get_client(&client_id)
            .await?
            .ok_or_else(|| AppError::Unauthorized("unknown client".to_string()))?;

        Ok(AuthenticatedClient(client))
    }
}
