//! Security primitives: a fixed-window per-key rate limiter, client-IP
//! extraction, IP hashing, and admin-token generation/checking.

use axum::http::HeaderMap;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Fixed-window rate limiter keyed by an arbitrary string (usually client IP).
pub struct RateLimiter {
    window: Duration,
    max: u32,
    hits: Mutex<HashMap<String, (Instant, u32)>>,
}

impl RateLimiter {
    pub fn new(max_per_window: u32, window_secs: u64) -> Self {
        RateLimiter {
            window: Duration::from_secs(window_secs),
            max: max_per_window,
            hits: Mutex::new(HashMap::new()),
        }
    }

    /// Returns true if the request is allowed; false if the limit is exceeded.
    pub fn check(&self, key: &str) -> bool {
        let now = Instant::now();
        let mut map = match self.hits.lock() {
            Ok(m) => m,
            Err(_) => return true, // fail-open on poisoned lock
        };

        // Opportunistic cleanup to bound memory.
        if map.len() > 10_000 {
            map.retain(|_, (start, _)| now.duration_since(*start) < self.window);
        }

        let entry = map.entry(key.to_string()).or_insert((now, 0));
        if now.duration_since(entry.0) >= self.window {
            *entry = (now, 0);
        }
        entry.1 += 1;
        entry.1 <= self.max
    }
}

/// Extract the best-effort client IP, honouring proxy headers used by
/// Railway / Vercel / Cloudflare, falling back to the socket address.
pub fn client_ip(headers: &HeaderMap, peer: Option<SocketAddr>) -> String {
    for h in ["x-forwarded-for", "x-real-ip", "cf-connecting-ip"] {
        if let Some(v) = headers.get(h).and_then(|v| v.to_str().ok()) {
            if let Some(first) = v.split(',').next() {
                let ip = first.trim();
                if !ip.is_empty() {
                    return ip.to_string();
                }
            }
        }
    }
    peer.map(|a| a.ip().to_string()).unwrap_or_else(|| "0.0.0.0".into())
}

/// Country code from a CDN/platform header, if present (instant, no lookup).
pub fn country_from_headers(headers: &HeaderMap) -> Option<String> {
    for h in ["cf-ipcountry", "x-vercel-ip-country", "x-country-code"] {
        if let Some(v) = headers.get(h).and_then(|v| v.to_str().ok()) {
            let v = v.trim();
            if !v.is_empty() && v != "XX" && v.to_lowercase() != "unknown" {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Salted, truncated SHA-256 of an IP — never store raw IPs.
pub fn hash_ip(ip: &str, salt: &str) -> String {
    let mut h = Sha256::new();
    h.update(salt.as_bytes());
    h.update(b"|");
    h.update(ip.as_bytes());
    let digest = h.finalize();
    hex16(&digest[..8])
}

fn hex16(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Constant-time-ish string compare for tokens.
pub fn token_matches(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for i in 0..a.len() {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

/// Generate a random hex token (used as admin token / IP salt when unset).
pub fn random_token() -> String {
    let mut buf = [0u8; 24];
    if getrandom::getrandom(&mut buf).is_err() {
        // Fallback: time-seeded (still unique per start).
        let t = crate::store::now_secs() as u64;
        for (i, b) in buf.iter_mut().enumerate() {
            *b = ((t >> (i % 8 * 8)) ^ (i as u64 * 1099511628211)) as u8;
        }
    }
    hex16(&buf)
}
