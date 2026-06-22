//! Statement Analyzer — fast, deterministic bank-statement analysis API,
//! with rate limiting, security headers, persistent analytics and an admin
//! dashboard.

mod currency;
mod engine;
mod extract;
mod geo;
mod model;
mod parsers;
mod security;
mod store;
mod util;

use axum::{
    extract::{ConnectInfo, DefaultBodyLimit, Multipart, Query, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use security::RateLimiter;
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::Mutex;
use std::collections::HashMap;
use store::Store;
use tower_http::cors::CorsLayer;

const INDEX_HTML: &str = include_str!("../static/index.html");
const ADMIN_HTML: &str = include_str!("../static/admin.html");

#[derive(Clone)]
struct AppState {
    store: Arc<Store>,
    analyze_limiter: Arc<RateLimiter>,
    feedback_limiter: Arc<RateLimiter>,
    admin_token: Arc<String>,
    ip_salt: Arc<String>,
    geo_cache: Arc<Mutex<HashMap<String, geo::GeoInfo>>>,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "statement_analyzer=info,tower_http=warn".into()),
        )
        .init();

    let db_path = std::env::var("DB_PATH").unwrap_or_else(|_| "data/analytics.db".into());
    let store = Arc::new(Store::open(&db_path).unwrap_or_else(|e| panic!("DB open {db_path}: {e}")));

    let admin_token = std::env::var("ADMIN_TOKEN").ok().filter(|t| !t.is_empty()).unwrap_or_else(|| {
        let t = security::random_token();
        tracing::warn!("ADMIN_TOKEN not set — generated temporary token: {t}");
        println!("\n  ⚠ ADMIN_TOKEN not set. Temporary admin token (set ADMIN_TOKEN to make it permanent):\n    {t}\n");
        t
    });
    let ip_salt = std::env::var("IP_SALT").ok().filter(|s| !s.is_empty()).unwrap_or_else(security::random_token);

    // Rate limits (override via env).
    let a_max = env_u32("RATE_ANALYZE_PER_MIN", 20);
    let f_max = env_u32("RATE_FEEDBACK_PER_MIN", 5);

    let state = AppState {
        store,
        analyze_limiter: Arc::new(RateLimiter::new(a_max, 60)),
        feedback_limiter: Arc::new(RateLimiter::new(f_max, 60)),
        admin_token: Arc::new(admin_token),
        ip_salt: Arc::new(ip_salt),
        geo_cache: Arc::new(Mutex::new(HashMap::new())),
    };

    let max_mb = env_u32("MAX_UPLOAD_MB", 200) as usize;

    let app = Router::new()
        .route("/", get(index))
        .route("/admin", get(admin_page))
        .route("/config.js", get(config_js))
        .route("/health", get(health))
        .route("/api/analyze", post(analyze))
        .route("/api/feedback", post(feedback))
        .route("/api/admin/stats", get(admin_stats))
        .layer(middleware::from_fn(security_headers))
        .layer(DefaultBodyLimit::max(max_mb * 1024 * 1024))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8000);
    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("could not bind {addr}: {e}"));

    tracing::info!("Statement Analyzer on http://localhost:{port}  (admin at /admin)");
    println!("  ▸ Statement Analyzer running at http://localhost:{port}  ·  admin: /admin\n");

    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .expect("server error");
}

fn env_u32(key: &str, default: u32) -> u32 {
    std::env::var(key).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

// ----------------------------- static routes -----------------------------

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn admin_page() -> Html<&'static str> {
    Html(ADMIN_HTML)
}

async fn config_js() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "application/javascript")],
        "window.API_BASE = \"\"; // same-origin (served by Rust binary)\n",
    )
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok", "service": "statement-analyzer", "version": env!("CARGO_PKG_VERSION") }))
}

// ----------------------------- security headers -----------------------------

async fn security_headers(req: axum::extract::Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    let set = |h: &mut HeaderMap, k: &'static str, v: &'static str| {
        h.insert(HeaderName::from_static(k), HeaderValue::from_static(v));
    };
    set(h, "x-content-type-options", "nosniff");
    set(h, "x-frame-options", "DENY");
    set(h, "referrer-policy", "strict-origin-when-cross-origin");
    set(h, "x-xss-protection", "0");
    set(h, "permissions-policy", "geolocation=(), microphone=(), camera=()");
    set(
        h,
        "content-security-policy",
        "default-src 'self'; script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data:; connect-src *; base-uri 'self'; form-action 'self'; frame-ancestors 'none'",
    );
    res
}

// ----------------------------- analyze -----------------------------

async fn analyze(
    State(st): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    mut multipart: Multipart,
) -> Response {
    let ip = security::client_ip(&headers, Some(peer));
    if !st.analyze_limiter.check(&ip) {
        return too_many();
    }

    let mut filename: Option<String> = None;
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut query = String::new();
    let mut visitor = String::new();

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return bad_request(format!("Malformed upload: {e}")),
        };
        match field.name().unwrap_or("") {
            "file" => {
                filename = field.file_name().map(|s| s.to_string());
                match field.bytes().await {
                    Ok(b) => file_bytes = Some(b.to_vec()),
                    Err(e) => return bad_request(format!("Could not read file: {e}")),
                }
            }
            "query" | "name" | "keyword" => query = field.text().await.unwrap_or_default(),
            "visitor" => visitor = field.text().await.unwrap_or_default(),
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let bytes = match file_bytes {
        Some(b) if !b.is_empty() => b,
        _ => return bad_request("No file uploaded (expected a `file` field).".into()),
    };
    let filename = sanitize_name(&filename.unwrap_or_else(|| "upload.bin".to_string()));
    let visitor = sanitize_visitor(&visitor);

    let result = match tokio::task::spawn_blocking({
        let fname = filename.clone();
        let q = query.clone();
        move || engine::run(&fname, &bytes, &q)
    })
    .await
    {
        Ok(r) => r,
        Err(e) => return bad_request(format!("Analysis task failed: {e}")),
    };

    // Record analytics (best-effort, never blocks the response on failure).
    let geo = resolve_geo(&st, &headers, &ip).await;
    st.store.record_event(&store::EventIn {
        visitor,
        ip_hash: security::hash_ip(&ip, &st.ip_salt),
        country: geo.country,
        region: geo.region,
        city: geo.city,
        file_kind: result.file.kind.clone(),
        size_bytes: result.file.size_bytes as i64,
        scanned: result.summary.total_transactions_scanned as i64,
        matched: result.summary.matched_transactions as i64,
        currency: result.currency.code.clone(),
    });

    (StatusCode::OK, Json(serde_json::to_value(result).unwrap())).into_response()
}

async fn resolve_geo(st: &AppState, headers: &HeaderMap, ip: &str) -> geo::GeoInfo {
    if let Some(cc) = security::country_from_headers(headers) {
        return geo::GeoInfo::just_country(&cc);
    }
    if let Ok(cache) = st.geo_cache.lock() {
        if let Some(g) = cache.get(ip) {
            return g.clone();
        }
    }
    let g = geo::lookup(ip).await;
    if let Ok(mut cache) = st.geo_cache.lock() {
        if cache.len() < 50_000 {
            cache.insert(ip.to_string(), g.clone());
        }
    }
    g
}

// ----------------------------- feedback -----------------------------

#[derive(Deserialize)]
struct FeedbackBody {
    visitor: Option<String>,
    stars: Option<i64>,
    review: Option<String>,
    would_pay: Option<String>,
    price: Option<String>,
}

async fn feedback(
    State(st): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<FeedbackBody>,
) -> Response {
    let ip = security::client_ip(&headers, Some(peer));
    if !st.feedback_limiter.check(&ip) {
        return too_many();
    }

    let stars = body.stars.unwrap_or(0).clamp(0, 5);
    let would_pay = match body.would_pay.unwrap_or_default().to_lowercase().as_str() {
        "yes" => "yes",
        "maybe" => "maybe",
        "no" => "no",
        _ => "",
    }
    .to_string();
    let review = clip(&body.review.unwrap_or_default(), 2000);
    let price = clip(&body.price.unwrap_or_default(), 60);

    if stars == 0 && would_pay.is_empty() && review.is_empty() {
        return bad_request("Empty feedback.".into());
    }

    let geo = resolve_geo(&st, &headers, &ip).await;
    st.store.record_feedback(&store::FeedbackIn {
        visitor: sanitize_visitor(&body.visitor.unwrap_or_default()),
        country: geo.country,
        stars,
        review,
        would_pay,
        price,
    });

    (StatusCode::OK, Json(json!({ "ok": true }))).into_response()
}

// ----------------------------- admin -----------------------------

async fn admin_stats(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let provided = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_start_matches("Bearer ").trim().to_string())
        .or_else(|| q.get("token").cloned())
        .unwrap_or_default();

    if !security::token_matches(&provided, &st.admin_token) {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "ok": false, "error": "Invalid admin token" }))).into_response();
    }

    let stats = st.store.stats();
    (StatusCode::OK, Json(serde_json::to_value(stats).unwrap())).into_response()
}

// ----------------------------- helpers -----------------------------

fn sanitize_name(s: &str) -> String {
    let base = s.rsplit(['/', '\\']).next().unwrap_or(s);
    clip(base, 200)
}
fn sanitize_visitor(s: &str) -> String {
    let v: String = s.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').take(64).collect();
    if v.is_empty() { "anon".into() } else { v }
}
fn clip(s: &str, max: usize) -> String {
    s.chars().take(max).collect::<String>().trim().to_string()
}

fn too_many() -> Response {
    (
        StatusCode::TOO_MANY_REQUESTS,
        Json(json!({ "ok": false, "error": "Rate limit exceeded — please slow down and try again shortly." })),
    )
        .into_response()
}

fn bad_request(msg: String) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "ok": false, "error": msg }))).into_response()
}
