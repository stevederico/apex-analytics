//! Per-site daily feedback budget (`FEEDBACK_DAILY_BUDGET`). Its own test
//! binary because it sets a process-wide env var.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use dottie_analytics::app::{build_app, AppState, VersionConfig};
use dottie_analytics::db::Db;
use serde_json::Value;
use tower::ServiceExt; // for `oneshot`

const ADMIN_KEY: &str = "test-admin-key-0000000000";
const READ_KEY: &str = "test-read-key-00000000000";
const ORIGIN: &str = "https://shop.example.com";

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_path(tag: &str) -> String {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!("dottie_feedback_{tag}_{}_{}.db", std::process::id(), n));
    path.to_string_lossy().into_owned()
}

fn cleanup(p: &str) {
    for f in [p.to_string(), format!("{p}-wal"), format!("{p}-shm")] {
        let _ = std::fs::remove_file(&f);
    }
}

/// App + one site; returns (app, site_id, write_key).
async fn setup(p: &str) -> (Router, String, String) {
    let db = Db::connect(p, HashSet::new(), HashSet::new()).await.unwrap();
    let site = db.create_site("Shop", Some("shop.example.com"), None).await.unwrap();
    let state = AppState::new(
        db,
        ADMIN_KEY.to_string(),
        VersionConfig {
            latest: "9.99".into(),
            min: "9.50".into(),
            download_url: "https://example.invalid/dl".into(),
            default_provider: "dottiepro".into(),
            provider_models: Vec::new(),
            vision_model: "grok-4.3".into(),
            local_model: "ggml-org/gemma-4-E4B-it-GGUF".into(),
            local_model_high_mem: "ggml-org/gemma-4-12B-it-GGUF".into(),
            stt_model_url_base: "https://example.invalid".into(),
        },
    )
    .with_read_key(READ_KEY.to_string());
    (
        build_app(state),
        site["id"].as_str().unwrap().to_string(),
        site["write_key"].as_str().unwrap().to_string(),
    )
}

fn json_post(ip: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/api/feedback")
        .header("x-forwarded-for", ip)
        .header("origin", ORIGIN)
        .header("user-agent", "Mozilla/5.0 Test")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn daily_budget_returns_429_with_retry_after() {
    std::env::set_var("FEEDBACK_DAILY_BUDGET", "2");
    let p = temp_path("budget");
    let (app, _, key) = setup(&p).await;
    for ip in ["203.0.113.70", "203.0.113.71"] {
        let resp = app
            .clone()
            .oneshot(json_post(ip, serde_json::json!({ "website": key, "message": "x" })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    // A fresh IP still hits the per-site budget.
    let resp = app
        .oneshot(json_post("203.0.113.72", serde_json::json!({ "website": key, "message": "x" })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(resp.headers()["retry-after"], "3600");
    assert_eq!(resp.headers()["access-control-allow-origin"], ORIGIN);
    cleanup(&p);
}
