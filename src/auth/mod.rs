use crate::error::AppError;
use axum::http::HeaderMap;
use chrono::{DateTime, Utc};
use dashmap::DashMap;
use hmac::{Hmac, Mac};
use rand::RngCore;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Mutex;
use subtle::ConstantTimeEq;

type HmacSha256 = Hmac<Sha256>;

pub fn extract_client_ip(
    headers: &HeaderMap,
    peer: SocketAddr,
    trust_proxy_headers: bool,
) -> IpAddr {
    if trust_proxy_headers {
        if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok())
            && let Some(first) = xff.split(',').next().map(str::trim)
            && let Ok(ip) = first.parse::<IpAddr>()
        {
            return ip;
        }
        if let Some(real_ip) = headers.get("x-real-ip").and_then(|v| v.to_str().ok())
            && let Ok(ip) = real_ip.trim().parse::<IpAddr>()
        {
            return ip;
        }
    }
    peer.ip()
}

pub fn hash_subnet(ip: IpAddr, hmac_secret: &[u8], now: DateTime<Utc>) -> String {
    let day_str = now.format("%Y-%m-%d").to_string();
    let mut mac = HmacSha256::new_from_slice(hmac_secret).expect("HMAC accepts any key length");
    mac.update(b"subnet:");
    mac.update(day_str.as_bytes());
    mac.update(b":");

    match ip {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            let masked = Ipv4Addr::new(a, b, c, 0);
            mac.update(&masked.octets());
        }
        IpAddr::V6(v6) => {
            let [a, b, c, _, _, _, _, _] = v6.segments();
            let masked = Ipv6Addr::new(a, b, c, 0, 0, 0, 0, 0);
            mac.update(&masked.octets());
        }
    }

    hex::encode(mac.finalize().into_bytes())
}

#[derive(Debug)]
pub struct RegistrationRateTracker {
    global_recent: Mutex<VecDeque<i64>>,
    subnet_recent: DashMap<String, VecDeque<i64>>,
}

impl Default for RegistrationRateTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl RegistrationRateTracker {
    pub fn new() -> Self {
        Self {
            global_recent: Mutex::new(VecDeque::new()),
            subnet_recent: DashMap::new(),
        }
    }

    pub fn record_registration(&self, subnet_hash: &str, now: i64) {
        {
            let mut g = self.global_recent.lock().expect("mutex poisoned");
            g.push_back(now);
            while let Some(&front) = g.front() {
                if now - front > 3600 {
                    g.pop_front();
                } else {
                    break;
                }
            }
        }
        let mut entry = self.subnet_recent.entry(subnet_hash.to_string()).or_default();
        entry.push_back(now);
        while let Some(&front) = entry.front() {
            if now - front > 3600 {
                entry.pop_front();
            } else {
                break;
            }
        }
    }

    /// Computes dynamic PoW difficulty bits based on global and `/24` subnet registration rates.
    pub fn difficulty_bits(
        &self,
        subnet_hash: &str,
        base_difficulty: u8,
        max_difficulty: u8,
        now: i64,
    ) -> u8 {
        let mut extra_bits: u8 = 0;

        // Global: compare last 5 minutes vs 1-hour average per 5 minutes (floor = 20)
        {
            let mut g = self.global_recent.lock().expect("mutex poisoned");
            while let Some(&front) = g.front() {
                if now - front > 3600 {
                    g.pop_front();
                } else {
                    break;
                }
            }
            let last_5m = g.iter().filter(|&&t| now - t <= 300).count() as f64;
            let baseline_5m = (g.len() as f64 / 12.0).max(20.0);
            if last_5m > 5.0 * baseline_5m {
                let ratio = last_5m / (5.0 * baseline_5m);
                extra_bits = extra_bits.max(ratio.log2().ceil().max(1.0) as u8);
            }
        }

        // Per-subnet: compare last 5 minutes vs 1-hour average per 5 minutes (floor = 3)
        if let Some(mut entry) = self.subnet_recent.get_mut(subnet_hash) {
            while let Some(&front) = entry.front() {
                if now - front > 3600 {
                    entry.pop_front();
                } else {
                    break;
                }
            }
            let last_5m = entry.iter().filter(|&&t| now - t <= 300).count() as f64;
            let baseline_5m = (entry.len() as f64 / 12.0).max(3.0);
            if last_5m > 5.0 * baseline_5m {
                let ratio = last_5m / (5.0 * baseline_5m);
                extra_bits = extra_bits.max(ratio.log2().ceil().max(1.0) as u8);
            }
        }

        base_difficulty.saturating_add(extra_bits).min(max_difficulty)
    }
}

pub fn issue_challenge(hmac_secret: &[u8], difficulty_bits: u8, now: i64) -> String {
    let mut rnd = [0u8; 16];
    rand::rng().fill_bytes(&mut rnd);
    let random_hex = hex::encode(rnd);
    let payload = format!("v1:{now}:{random_hex}:{difficulty_bits}");

    let mut mac = HmacSha256::new_from_slice(hmac_secret).expect("HMAC accepts any key length");
    mac.update(b"pow:");
    mac.update(payload.as_bytes());
    let sig = hex::encode(mac.finalize().into_bytes());

    format!("{payload}:{sig}")
}

pub fn verify_challenge_and_pow(
    challenge: &str,
    nonce: &str,
    hmac_secret: &[u8],
    ttl_secs: i64,
    now: i64,
) -> Result<(), AppError> {
    let parts: Vec<&str> = challenge.split(':').collect();
    if parts.len() != 5 || parts[0] != "v1" {
        return Err(AppError::BadRequest("invalid challenge format".to_string()));
    }

    let ts: i64 = parts[1]
        .parse()
        .map_err(|_| AppError::BadRequest("invalid challenge timestamp".to_string()))?;
    let random_hex = parts[2];
    let difficulty_bits: u8 = parts[3]
        .parse()
        .map_err(|_| AppError::BadRequest("invalid challenge difficulty".to_string()))?;
    let sig_bytes = hex::decode(parts[4])
        .map_err(|_| AppError::BadRequest("invalid challenge signature encoding".to_string()))?;

    let payload = format!("v1:{ts}:{random_hex}:{difficulty_bits}");
    let mut mac = HmacSha256::new_from_slice(hmac_secret).expect("HMAC accepts any key length");
    mac.update(b"pow:");
    mac.update(payload.as_bytes());
    let expected = mac.finalize().into_bytes();

    if sig_bytes.len() != expected.len() || !bool::from(sig_bytes.ct_eq(expected.as_slice())) {
        return Err(AppError::Unauthorized("invalid challenge signature".to_string()));
    }

    if now - ts > ttl_secs || ts - now > 30 {
        return Err(AppError::Unauthorized("challenge expired".to_string()));
    }

    let mut hasher = Sha256::new();
    hasher.update(challenge.as_bytes());
    hasher.update(b":");
    hasher.update(nonce.as_bytes());
    let digest = hasher.finalize();

    if leading_zero_bits(&digest) < difficulty_bits as u32 {
        return Err(AppError::Forbidden(format!(
            "insufficient proof-of-work: required {difficulty_bits} leading zero bits"
        )));
    }

    Ok(())
}

pub fn leading_zero_bits(bytes: &[u8]) -> u32 {
    let mut zeros = 0u32;
    for &b in bytes {
        if b == 0 {
            zeros += 8;
        } else {
            zeros += b.leading_zeros();
            break;
        }
    }
    zeros
}

pub fn solve_pow_for_test(challenge: &str, difficulty_bits: u8) -> String {
    let mut nonce = 0u64;
    loop {
        let candidate = nonce.to_string();
        let mut hasher = Sha256::new();
        hasher.update(challenge.as_bytes());
        hasher.update(b":");
        hasher.update(candidate.as_bytes());
        let digest = hasher.finalize();
        if leading_zero_bits(&digest) >= difficulty_bits as u32 {
            return candidate;
        }
        nonce += 1;
    }
}

pub fn issue_bearer_token(client_id: &str, hmac_secret: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(hmac_secret).expect("HMAC accepts any key length");
    mac.update(b"token:");
    mac.update(client_id.as_bytes());
    let sig = hex::encode(mac.finalize().into_bytes());
    format!("qt_{client_id}.{sig}")
}

pub fn verify_bearer_token(token: &str, hmac_secret: &[u8]) -> Option<String> {
    let stripped = token.strip_prefix("qt_")?;
    let (client_id, sig_hex) = stripped.split_once('.')?;
    let sig_bytes = hex::decode(sig_hex).ok()?;

    let mut mac = HmacSha256::new_from_slice(hmac_secret).expect("HMAC accepts any key length");
    mac.update(b"token:");
    mac.update(client_id.as_bytes());
    let expected = mac.finalize().into_bytes();

    if sig_bytes.len() == expected.len() && bool::from(sig_bytes.ct_eq(expected.as_slice())) {
        Some(client_id.to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn test_subnet_hash_masks_v4_and_v6_and_rotates_daily() {
        let secret = b"super-secret";
        let day1 = Utc.with_ymd_and_hms(2026, 11, 17, 10, 0, 0).unwrap();
        let day2 = Utc.with_ymd_and_hms(2026, 11, 18, 10, 0, 0).unwrap();

        let ip1: IpAddr = "192.168.10.5".parse().unwrap();
        let ip2: IpAddr = "192.168.10.254".parse().unwrap();
        let ip3: IpAddr = "192.168.11.5".parse().unwrap();

        assert_eq!(
            hash_subnet(ip1, secret, day1),
            hash_subnet(ip2, secret, day1)
        );
        assert_ne!(
            hash_subnet(ip1, secret, day1),
            hash_subnet(ip3, secret, day1)
        );
        assert_ne!(
            hash_subnet(ip1, secret, day1),
            hash_subnet(ip1, secret, day2)
        );

        let v6_1: IpAddr = "2001:db8:abcd:1234::1".parse().unwrap();
        let v6_2: IpAddr = "2001:db8:abcd:9999::2".parse().unwrap();
        assert_eq!(
            hash_subnet(v6_1, secret, day1),
            hash_subnet(v6_2, secret, day1)
        );
    }

    #[test]
    fn test_pow_challenge_and_bearer_token() {
        let secret = b"test-secret";
        let now = 1_700_000_000;
        let challenge = issue_challenge(secret, 8, now);
        let nonce = solve_pow_for_test(&challenge, 8);

        verify_challenge_and_pow(&challenge, &nonce, secret, 120, now + 10).unwrap();
        assert!(verify_challenge_and_pow(&challenge, &nonce, secret, 120, now + 121).is_err());
        assert!(verify_challenge_and_pow(&challenge, "bad", secret, 120, now + 10).is_err());

        let token = issue_bearer_token("abc-123", secret);
        assert_eq!(
            verify_bearer_token(&token, secret).as_deref(),
            Some("abc-123")
        );
        assert!(verify_bearer_token("qt_abc-123.deadbeef", secret).is_none());
    }

    #[test]
    fn test_dynamic_difficulty_scales_on_spike() {
        let tracker = RegistrationRateTracker::new();
        let now = 1_700_000_000;
        assert_eq!(tracker.difficulty_bits("sub1", 20, 26, now), 20);

        for i in 0..25 {
            tracker.record_registration("sub1", now - i);
        }
        let scaled = tracker.difficulty_bits("sub1", 20, 26, now);
        assert!(scaled > 20 && scaled <= 26);
    }
}
