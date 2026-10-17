//! dottie-analytics — Rust port on the mainstream axum + tokio + sqlx stack.
//!
//! This scaffold boots an axum server with a /health route to prove the
//! toolchain, and constructs the sqlx SqlitePool + runs migrations at startup.
//! The ingest/report endpoints from backend/server.js get ported on top, one
//! router/handler at a time, with the `db` module talking to the same SQLite
//! file.

use std::collections::HashSet;

use dottie_analytics::app::{build_app, AppState};
use dottie_analytics::db::Db;
use dottie_analytics::github;
use dottie_analytics::reports::ReportExclude;

#[tokio::main]
async fn main() {
    // Structured logging via tracing; RUST_LOG controls level (default info).
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    // DB path: DATABASE_URL (strip any sqlite:// scheme) or DB_PATH, else the
    // default production file. Mirrors db.js's DB_PATH default.
    let db_path = std::env::var("DATABASE_URL")
        .map(|u| u.trim_start_matches("sqlite://").to_string())
        .or_else(|_| std::env::var("DB_PATH"))
        .unwrap_or_else(|_| "./databases/dottie-api.db".to_string());

    // Ensure the parent directory exists (create_if_missing only makes the file).
    if let Some(parent) = std::path::Path::new(&db_path).parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Exclusion sets parsed from env here (NOT inside db.rs) and injected. Emails
    // are lowercased on the way in so matching is case-insensitive; install_ids
    // match exactly. Both normally empty — the standing filter is the smoke-test
    // pattern baked into db.rs.
    let excluded_install_ids = parse_set("DOTTIE_EXCLUDED_INSTALL_IDS", false);
    let excluded_emails = parse_set("DOTTIE_EXCLUDED_EMAILS", true);

    let db = Db::connect(&db_path, excluded_install_ids, excluded_emails)
        .await
        .expect("connect db + run migrations")
        // Operator noise hidden from daily-report person lists only.
        .with_report_exclude(ReportExclude::from_env());
    tracing::info!("db ready at {db_path}");
    // ENABLED bit only — never the secret. A half-configured deploy that
    // forgot PRO_JWT_SECRET keeps minting opaque tokens and the split relay
    // cannot verify them.
    tracing::info!(jwt_signing = !db.jwt_secret.is_empty(), "pro token signing");

    let state = AppState::from_env(db);
    // Growth-chart collectors: seed repos, snapshot downloads/metrics, hourly backfill.
    github::start_collectors(state.db.clone(), state.github.clone());
    let app = build_app(state);

    let port = std::env::var("PORT").unwrap_or_else(|_| "8080".to_string());
    // [::] = dual-stack (v4+v6) — Railway's edge proxies over IPv6; 0.0.0.0 502s
    let addr = format!("[::]:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .expect("bind listener");
    tracing::info!("listening on http://{addr}");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .expect("serve");
}

/// Resolve when the process should drain and exit: either a Ctrl-C (SIGINT) or a
/// SIGTERM. Railway sends SIGTERM on every deploy/restart, so handling it lets
/// axum finish in-flight requests and close the SqlitePool cleanly instead of
/// being killed mid-write. On non-unix targets only Ctrl-C is wired up.
async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => tracing::info!("received Ctrl-C (SIGINT), shutting down gracefully"),
        () = terminate => tracing::info!("received SIGTERM, shutting down gracefully"),
    }
}

/// Parse a comma-separated env var into a trimmed, non-empty `HashSet`,
/// lowercasing entries when `lower` (used for the email set).
fn parse_set(var: &str, lower: bool) -> HashSet<String> {
    std::env::var(var)
        .unwrap_or_default()
        .split(',')
        .map(|s| {
            let t = s.trim();
            if lower {
                t.to_ascii_lowercase()
            } else {
                t.to_string()
            }
        })
        .filter(|s| !s.is_empty())
        .collect()
}
