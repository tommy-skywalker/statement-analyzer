//! Statement Analyzer — fast, deterministic bank-statement analysis API.
//!
//! Routes:
//!   GET  /            -> single-page UI
//!   GET  /health      -> liveness probe
//!   POST /api/analyze -> multipart { file, query } -> JSON analysis

mod currency;
mod engine;
mod extract;
mod model;
mod parsers;
mod util;

use axum::{
    extract::{DefaultBodyLimit, Multipart},
    http::StatusCode,
    response::{Html, IntoResponse, Json},
    routing::{get, post},
    Router,
};
use serde_json::json;
use tower_http::cors::CorsLayer;

const INDEX_HTML: &str = include_str!("../static/index.html");
const MAX_UPLOAD: usize = 1024 * 1024 * 1024; // 1 GiB

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "statement_analyzer=info,tower_http=warn".into()),
        )
        .init();

    let app = Router::new()
        .route("/", get(index))
        .route("/config.js", get(config_js))
        .route("/health", get(health))
        .route("/api/analyze", post(analyze))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD))
        .layer(CorsLayer::permissive());

    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8000);
    let addr = format!("0.0.0.0:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .unwrap_or_else(|e| panic!("could not bind {addr}: {e}"));

    tracing::info!("Statement Analyzer listening on http://localhost:{port}");
    println!("\n  ▸ Statement Analyzer running at http://localhost:{port}\n");

    axum::serve(listener, app).await.expect("server error");
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

/// When the Rust binary serves the page directly, the API is same-origin, so
/// the config just leaves API_BASE blank. (On Vercel, static/config.js is used.)
async fn config_js() -> impl IntoResponse {
    (
        [(axum::http::header::CONTENT_TYPE, "application/javascript")],
        "window.API_BASE = \"\"; // same-origin (served by Rust binary)\n",
    )
}

async fn health() -> impl IntoResponse {
    Json(json!({ "status": "ok", "service": "statement-analyzer", "version": env!("CARGO_PKG_VERSION") }))
}

async fn analyze(mut multipart: Multipart) -> impl IntoResponse {
    let mut filename: Option<String> = None;
    let mut file_bytes: Option<Vec<u8>> = None;
    let mut query = String::new();

    loop {
        let field = match multipart.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return bad_request(format!("Malformed upload: {e}")),
        };
        let name = field.name().unwrap_or("").to_string();
        match name.as_str() {
            "file" => {
                filename = field.file_name().map(|s| s.to_string());
                match field.bytes().await {
                    Ok(b) => file_bytes = Some(b.to_vec()),
                    Err(e) => return bad_request(format!("Could not read file: {e}")),
                }
            }
            "query" | "name" | "keyword" => {
                query = field.text().await.unwrap_or_default();
            }
            _ => {
                let _ = field.bytes().await;
            }
        }
    }

    let bytes = match file_bytes {
        Some(b) if !b.is_empty() => b,
        _ => return bad_request("No file uploaded (expected a `file` field).".into()),
    };
    let filename = filename.unwrap_or_else(|| "upload.bin".to_string());

    // Run the CPU-bound analysis off the async runtime.
    let result = tokio::task::spawn_blocking(move || engine::run(&filename, &bytes, &query)).await;

    match result {
        Ok(r) => (StatusCode::OK, Json(serde_json::to_value(r).unwrap())).into_response(),
        Err(e) => bad_request(format!("Analysis task failed: {e}")),
    }
}

fn bad_request(msg: String) -> axum::response::Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "ok": false, "error": msg })),
    )
        .into_response()
}
