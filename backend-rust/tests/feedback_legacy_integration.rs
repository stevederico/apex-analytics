//! Retired feedback-assistant compatibility (src/feedback_legacy.rs): old
//! widget URLs, the `/v1` two-step screenshot + submission API, and `pk_…`
//! keys on `/api/feedback`. Own test binary because `FEEDBACK_LEGACY_KEYS` is a
//! process-wide env var.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use apex_analytics::app::{build_app, AppState, VersionConfig};
use apex_analytics::db::Db;
use http_body_util::BodyExt;
use serde_json::Value;
use tower::ServiceExt; // for `oneshot`

const ADMIN_KEY: &str = "test-admin-key-0000000000";
const ORIGIN: &str = "https://care.example.com";
const PK: &str = "pk_legacy_test_key";
const SITE_ID: &str = "site-legacy-1";

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_path(tag: &str) -> String {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!("dottie_fb_legacy_{tag}_{}_{}.db", std::process::id(), n));
    path.to_string_lossy().into_owned()
}

fn cleanup(p: &str) {
    for f in [p.to_string(), format!("{p}-wal"), format!("{p}-shm")] {
        let _ = std::fs::remove_file(&f);
    }
}

async fn setup(p: &str) -> Router {
    std::env::set_var("FEEDBACK_LEGACY_KEYS", format!("pk_other=nowhere, {PK}={SITE_ID}"));
    let db = Db::connect(p, HashSet::new(), HashSet::new()).await.unwrap();
    db.create_site("Care", Some("care.example.com"), Some(SITE_ID)).await.unwrap();
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
    );
    build_app(state)
}

fn req(method: &str, uri: &str, ip: &str, headers: &[(&str, &str)], body: Body) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("x-forwarded-for", ip)
        .header("origin", ORIGIN)
        .header("user-agent", "Mozilla/5.0 Test");
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(body).unwrap()
}

fn json_body(v: Value) -> Body {
    Body::from(v.to_string())
}

fn png_multipart() -> (String, Vec<u8>) {
    let b = "----legacyBoundary";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"s.png\"\r\nContent-Type: image/png\r\n\r\n").as_bytes());
    body.extend_from_slice(b"\x89PNG\r\n\x1a\nfake");
    body.extend_from_slice(format!("\r\n--{b}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={b}"), body)
}

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    resp.into_body().collect().await.unwrap().to_bytes().to_vec()
}

async fn body_json(resp: axum::response::Response) -> Value {
    serde_json::from_slice(&body_bytes(resp).await).unwrap()
}

async fn admin_rows(app: &Router) -> Vec<Value> {
    let r = Request::builder()
        .uri("/api/stats/feedback?all=1")
        .header("x-forwarded-for", "198.51.100.1")
        .header("authorization", format!("Bearer {ADMIN_KEY}"))
        .body(Body::empty())
        .unwrap();
    body_json(app.clone().oneshot(r).await.unwrap()).await["rows"].as_array().unwrap().clone()
}

#[tokio::test]
async fn old_widget_urls_serve_the_current_widget() {
    let p = temp_path("urls");
    let app = setup(&p).await;
    for uri in ["/widget.js", "/widget/v3.5.0.js", "/widget/v3.24.0.js"] {
        let resp = app.clone().oneshot(req("GET", uri, "203.0.113.1", &[], Body::empty())).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        let js = String::from_utf8(body_bytes(resp).await).unwrap();
        assert!(js.contains("data-project"), "{uri} must read the old tag's key");
    }
    let resp = app.clone().oneshot(req("GET", "/widget/html2canvas-v1.js", "203.0.113.1", &[], Body::empty())).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app.clone().oneshot(req("GET", "/widget/other.js", "203.0.113.1", &[], Body::empty())).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = app.clone().oneshot(req("GET", &format!("/v1/projects/{PK}/widget"), "203.0.113.1", &[], Body::empty())).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["greeting"], Value::Null);
    let resp = app.oneshot(req("GET", &format!("/v1/projects/{PK}/changelog"), "203.0.113.1", &[], Body::empty())).await.unwrap();
    assert_eq!(body_json(resp).await["changelog"], serde_json::json!([]));
    cleanup(&p);
}

#[tokio::test]
async fn v1_screenshot_then_submission_writes_one_row() {
    let p = temp_path("v1");
    let app = setup(&p).await;

    let resp = app.clone().oneshot(req("OPTIONS", "/v1/submissions", "203.0.113.2", &[], Body::empty())).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert!(resp.headers()["access-control-allow-headers"].to_str().unwrap().contains("X-Project-Key"));
    assert_eq!(resp.headers()["access-control-allow-origin"], ORIGIN);

    let (ct, body) = png_multipart();
    let resp = app
        .clone()
        .oneshot(req("POST", "/v1/screenshots", "203.0.113.2", &[("x-project-key", PK), ("content-type", &ct)], Body::from(body)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let shot = body_json(resp).await["screenshotId"].as_str().unwrap().to_string();

    let sub = serde_json::json!({
        "message": "old widget says hi", "url": "https://care.example.com/x", "userAgent": "OldUA/1",
        "appVersion": "3.5.0", "endUserId": "u9", "endUserName": "Bo", "endUserEmail": "bo@example.com",
        "screenshotId": shot,
    });
    let resp = app
        .clone()
        .oneshot(req("POST", "/v1/submissions", "203.0.113.2", &[("x-project-key", PK), ("content-type", "application/json")], json_body(sub.clone())))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["access-control-allow-origin"], ORIGIN);
    let j = body_json(resp).await;
    assert_eq!(j["ok"], true);
    assert!(j["submissionId"].is_string());

    let rows = admin_rows(&app).await;
    assert_eq!(rows.len(), 1);
    let r = &rows[0];
    assert_eq!(r["site_id"], SITE_ID);
    assert_eq!(r["user_agent"], "OldUA/1");
    assert_eq!(r["end_user_email"], "bo@example.com");
    assert_eq!(r["app_version"], "3.5.0");
    assert_eq!(r["has_screenshot"], true);

    // A screenshot id is single-use.
    let resp = app
        .oneshot(req("POST", "/v1/submissions", "203.0.113.3", &[("x-project-key", PK), ("content-type", "application/json")], json_body(sub)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["error"], "unknown_screenshot");
    cleanup(&p);
}

#[tokio::test]
async fn v1_rejects_unknown_keys_and_bad_input() {
    let p = temp_path("v1bad");
    let app = setup(&p).await;
    let msg = serde_json::json!({ "message": "x" });
    for key in [None, Some("pk_nope"), Some("pk_other")] {
        let mut h = vec![("content-type", "application/json")];
        if let Some(k) = key {
            h.push(("x-project-key", k));
        }
        let resp = app.clone().oneshot(req("POST", "/v1/submissions", "203.0.113.4", &h, json_body(msg.clone()))).await.unwrap();
        // pk_other maps to a site that does not exist: the row is refused, not orphaned.
        assert_ne!(resp.status(), StatusCode::OK, "{key:?}");
    }
    let resp = app
        .clone()
        .oneshot(req("POST", "/v1/submissions", "203.0.113.5", &[("x-project-key", PK)], json_body(serde_json::json!({ "message": "  " }))))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = app
        .clone()
        .oneshot(req("POST", "/v1/submissions", "203.0.113.5", &[("x-project-key", PK)], json_body(serde_json::json!({ "message": "x".repeat(5001) }))))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    let resp = app
        .clone()
        .oneshot(req("POST", "/v1/submissions", "203.0.113.6", &[("x-project-key", PK)], json_body(serde_json::json!({ "message": "x", "screenshotId": "made-up" }))))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert!(admin_rows(&app).await.is_empty());
    cleanup(&p);
}

#[tokio::test]
async fn new_endpoint_accepts_a_legacy_key() {
    let p = temp_path("pk");
    let app = setup(&p).await;
    let resp = app
        .clone()
        .oneshot(req("POST", "/api/feedback", "203.0.113.7", &[("content-type", "application/json")], json_body(serde_json::json!({ "website": PK, "message": "via new path" }))))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let rows = admin_rows(&app).await;
    assert_eq!(rows[0]["site_id"], SITE_ID);
    cleanup(&p);
}
