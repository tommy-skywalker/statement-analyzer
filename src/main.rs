//! Statement Analyzer — fast, deterministic bank-statement analysis API,
//! with rate limiting, security headers, persistent analytics and an admin
//! dashboard.

mod currency;
mod engine;
mod extract;
mod geo;
mod model;
mod nlquery;
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
const APP_HTML: &str = include_str!("../static/app.html");
const ADMIN_HTML: &str = include_str!("../static/admin.html");

#[derive(Clone)]
struct AppState {
    store: Arc<Store>,
    analyze_limiter: Arc<RateLimiter>,
    feedback_limiter: Arc<RateLimiter>,
    login_limiter: Arc<RateLimiter>,
    admin_user: Arc<String>,
    admin_password: Arc<String>,
    admin_token: Arc<String>,
    ip_salt: Arc<String>,
    geo_cache: Arc<Mutex<HashMap<String, geo::GeoInfo>>>,
    analysis_cache: Arc<Mutex<AnalysisCache>>,
    // Short-lived admin session tokens (token -> issued-at). The permanent
    // admin_token is never sent to the browser anymore; the UI gets one of these.
    admin_sessions: Arc<Mutex<HashMap<String, std::time::Instant>>>,
    session_ttl: std::time::Duration,
}

/// LRU-ish cache of parsed statements so keyword changes skip re-parsing.
struct AnalysisCache {
    map: HashMap<String, std::sync::Arc<engine::ParsedDoc>>,
    order: std::collections::VecDeque<String>,
    cap: usize,
}
impl AnalysisCache {
    fn new(cap: usize) -> Self {
        AnalysisCache { map: HashMap::new(), order: std::collections::VecDeque::new(), cap }
    }
    fn insert(&mut self, id: String, doc: std::sync::Arc<engine::ParsedDoc>) {
        self.map.insert(id.clone(), doc);
        self.order.push_back(id);
        while self.order.len() > self.cap {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }
    fn get(&self, id: &str) -> Option<std::sync::Arc<engine::ParsedDoc>> {
        self.map.get(id).cloned()
    }
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

    // Admin login: username + password (env). Password is generated & logged if unset.
    let admin_user = std::env::var("ADMIN_USER").ok().filter(|u| !u.is_empty()).unwrap_or_else(|| "admin".into());
    let admin_password = std::env::var("ADMIN_PASSWORD").ok().filter(|p| !p.is_empty()).unwrap_or_else(|| {
        let p = security::random_token();
        tracing::warn!("ADMIN_PASSWORD not set — generated temporary password");
        println!("\n  ⚠ ADMIN_PASSWORD not set. Log in at /admin with:\n    username: {admin_user}\n    password: {p}\n  (set ADMIN_PASSWORD to make it permanent)\n");
        p
    });
    // Internal bearer token used by the dashboard after login (not user-facing).
    let admin_token = std::env::var("ADMIN_TOKEN").ok().filter(|t| !t.is_empty()).unwrap_or_else(security::random_token);
    let ip_salt = std::env::var("IP_SALT").ok().filter(|s| !s.is_empty()).unwrap_or_else(security::random_token);

    // Rate limits (override via env).
    let a_max = env_u32("RATE_ANALYZE_PER_MIN", 20);
    let f_max = env_u32("RATE_FEEDBACK_PER_MIN", 5);

    let state = AppState {
        store: store.clone(),
        analyze_limiter: Arc::new(RateLimiter::new(a_max, 60)),
        feedback_limiter: Arc::new(RateLimiter::new(f_max, 60)),
        login_limiter: Arc::new(RateLimiter::new(10, 60)),
        admin_user: Arc::new(admin_user),
        admin_password: Arc::new(admin_password),
        admin_token: Arc::new(admin_token),
        ip_salt: Arc::new(ip_salt),
        geo_cache: Arc::new(Mutex::new(HashMap::new())),
        analysis_cache: Arc::new(Mutex::new(AnalysisCache::new(32))),
        admin_sessions: Arc::new(Mutex::new(HashMap::new())),
        session_ttl: std::time::Duration::from_secs(env_u32("SESSION_TTL_HOURS", 12) as u64 * 3600),
    };

    let max_mb = env_u32("MAX_UPLOAD_MB", 25) as usize;

    // Background retention: prune events older than N days, on boot and daily.
    {
        let store = store.clone();
        let days = env_u32("EVENTS_RETENTION_DAYS", 365) as i64;
        let removed = store.prune(days);
        if removed > 0 {
            tracing::info!("pruned {removed} events older than {days} days");
        }
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(6 * 3600));
            loop {
                tick.tick().await;
                let n = tokio::task::block_in_place(|| store.prune(days));
                if n > 0 {
                    tracing::info!("pruned {n} old events");
                }
            }
        });
    }

    // CORS: lock to ALLOWED_ORIGINS (comma-separated) in production; permissive if unset.
    let cors = match std::env::var("ALLOWED_ORIGINS") {
        Ok(v) if !v.trim().is_empty() => {
            let origins: Vec<axum::http::HeaderValue> =
                v.split(',').filter_map(|s| s.trim().parse().ok()).collect();
            tracing::info!("CORS locked to: {v}");
            CorsLayer::new()
                .allow_origin(origins)
                .allow_methods(tower_http::cors::Any)
                .allow_headers(tower_http::cors::Any)
        }
        _ => CorsLayer::permissive(),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/app", get(app_page))
        .route("/admin", get(admin_page))
        .route("/config.js", get(config_js))
        .route("/favicon.svg", get(favicon))
        .route("/og.png", get(og_image))
        .route("/health", get(health))
        .route("/api/analyze", post(analyze))
        .route("/api/feedback", post(feedback))
        .route("/api/admin/login", post(admin_login))
        .route("/api/admin/stats", get(admin_stats))
        .layer(middleware::from_fn(security_headers))
        .layer(tower_http::compression::CompressionLayer::new())
        .layer(DefaultBodyLimit::max(max_mb * 1024 * 1024))
        .layer(cors)
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

async fn app_page() -> Html<&'static str> {
    Html(APP_HTML)
}

async fn favicon() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "image/svg+xml")],
        include_str!("../static/favicon.svg"),
    )
}

async fn og_image() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "image/png")],
        include_bytes!("../static/og.png").as_slice(),
    )
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
    // Always serve fresh HTML/assets so a reload never shows a stale UI.
    set(h, "cache-control", "no-store, max-age=0, must-revalidate");
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

    let t0 = std::time::Instant::now();
    let mut filename: Option<String> = None;
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut query = String::new();
    let mut visitor = String::new();
    let mut cache_id = String::new();

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
            "cache_id" => cache_id = field.text().await.unwrap_or_default(),
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    // ---- Fast path: re-filter a previously parsed doc (keyword change) ----
    if file_bytes.as_ref().map(|b| b.is_empty()).unwrap_or(true) && !cache_id.is_empty() {
        let cached = st.analysis_cache.lock().ok().and_then(|c| c.get(&cache_id));
        match cached {
            Some(doc) => {
                let q = query.clone();
                let mut result = match tokio::task::spawn_blocking(move || engine::analyze_parsed(&doc, &q)).await {
                    Ok(r) => r,
                    Err(e) => return bad_request(format!("Analysis task failed: {e}")),
                };
                result.cache_id = Some(cache_id);
                return (StatusCode::OK, Json(serde_json::to_value(result).unwrap())).into_response();
            }
            // Cache expired: tell the client to resend the file.
            None => {
                return (
                    StatusCode::OK,
                    Json(json!({ "ok": false, "cache_miss": true, "error": "Session expired, re-analysing file." })),
                )
                    .into_response();
            }
        }
    }

    // ---- Slow path: parse a freshly uploaded file ----
    let bytes = match file_bytes {
        Some(b) if !b.is_empty() => b,
        _ => return bad_request("No file uploaded (expected a `file` field).".into()),
    };
    let filename = sanitize_name(&filename.unwrap_or_else(|| "upload.bin".to_string()));
    let visitor = sanitize_visitor(&visitor);
    let size = bytes.len();

    let (mut result, doc_opt) = match tokio::task::spawn_blocking({
        let fname = filename.clone();
        let q = query.clone();
        move || engine::parse_and_analyze(&fname, &bytes, &q)
    })
    .await
    {
        Ok(pair) => pair,
        Err(e) => return bad_request(format!("Analysis task failed: {e}")),
    };

    // Cache the parsed doc so keyword changes re-filter instantly.
    if let Some(doc) = doc_opt {
        let id = security::random_token();
        if let Ok(mut cache) = st.analysis_cache.lock() {
            cache.insert(id.clone(), std::sync::Arc::new(doc));
        }
        result.cache_id = Some(id);
    }

    // Real elapsed = parse + analyze.
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    result.elapsed_ms = (ms * 100.0).round() / 100.0;
    let secs = t0.elapsed().as_secs_f64();
    result.throughput_mb_s = if secs > 0.0 {
        (((size as f64 / (1024.0 * 1024.0)) / secs) * 100.0).round() / 100.0
    } else {
        0.0
    };

    // Record analytics off the hot path (only on a fresh parse) — the geo
    // lookup can take up to ~1.8s and must never delay the user's response.
    {
        let st2 = st.clone();
        let headers2 = headers.clone();
        let ip_hash = security::hash_ip(&ip, &st.ip_salt);
        let ev = store::EventIn {
            visitor,
            ip_hash,
            country: String::new(),
            region: String::new(),
            city: String::new(),
            file_kind: result.file.kind.clone(),
            size_bytes: result.file.size_bytes as i64,
            scanned: result.summary.total_transactions_scanned as i64,
            matched: result.summary.matched_transactions as i64,
            currency: result.currency.code.clone(),
        };
        tokio::spawn(async move {
            let geo = resolve_geo(&st2, &headers2, &ip).await;
            st2.store.record_event(&store::EventIn { country: geo.country, region: geo.region, city: geo.city, ..ev });
        });
    }

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

#[derive(Deserialize)]
struct LoginBody {
    username: Option<String>,
    password: Option<String>,
}

/// Validate username + password; on success return the internal bearer token
/// the dashboard then uses for /api/admin/stats.
async fn admin_login(
    State(st): State<AppState>,
    headers: HeaderMap,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<LoginBody>,
) -> Response {
    let ip = security::client_ip(&headers, Some(peer));
    if !st.login_limiter.check(&ip) {
        return too_many();
    }
    let user = body.username.unwrap_or_default();
    let pass = body.password.unwrap_or_default();
    let ok = security::token_matches(&user, &st.admin_user)
        && security::token_matches(&pass, &st.admin_password);
    if ok {
        // Issue a short-lived session token (the permanent admin_token is never
        // exposed to the browser).
        let session = security::random_token();
        if let Ok(mut s) = st.admin_sessions.lock() {
            let now = std::time::Instant::now();
            s.retain(|_, &mut issued| now.duration_since(issued) < st.session_ttl);
            s.insert(session.clone(), now);
        }
        (StatusCode::OK, Json(json!({ "ok": true, "token": session }))).into_response()
    } else {
        (StatusCode::UNAUTHORIZED, Json(json!({ "ok": false, "error": "Invalid username or password" }))).into_response()
    }
}

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

    // Accept a valid (unexpired) session token, or the permanent admin_token
    // for scripted/API access.
    let valid_session = st.admin_sessions.lock().ok().map_or(false, |mut s| {
        let now = std::time::Instant::now();
        s.retain(|_, &mut issued| now.duration_since(issued) < st.session_ttl);
        s.get(&provided).is_some()
    });
    if !valid_session && !security::token_matches(&provided, &st.admin_token) {
        return (StatusCode::UNAUTHORIZED, Json(json!({ "ok": false, "error": "Invalid or expired session" }))).into_response();
    }

    // Run the (potentially heavy) aggregation off the async runtime.
    let store = st.store.clone();
    let stats = match tokio::task::spawn_blocking(move || store.stats()).await {
        Ok(s) => s,
        Err(_) => return bad_request("Stats query failed.".into()),
    };
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
