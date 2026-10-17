//! In-app feedback (src/feedback.rs): widget asset, public ingest (JSON +
//! multipart screenshot), size limits incl. 413, rate limit, admin list,
//! screenshot fetch and status triage.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use dottie_analytics::app::{build_app, AppState, VersionConfig};
use dottie_analytics::db::Db;
use dottie_analytics::feedback::{MAX_MESSAGE_CHARS, MAX_SCREENSHOT_BYTES};
use http_body_util::BodyExt;
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

/// A browser-shaped FormData body: text fields, then an optional `file` part.
fn multipart_post(ip: &str, fields: &[(&str, &str)], file: Option<(&str, &[u8])>) -> Request<Body> {
    let b = "----dottieTestBoundary";
    let mut body = Vec::new();
    for (k, v) in fields {
        body.extend_from_slice(
            format!("--{b}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes(),
        );
    }
    if let Some((mime, bytes)) = file {
        body.extend_from_slice(
            format!(
                "--{b}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"screenshot.jpg\"\r\nContent-Type: {mime}\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(bytes);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{b}--\r\n").as_bytes());
    Request::builder()
        .method("POST")
        .uri("/api/feedback")
        .header("x-forwarded-for", ip)
        .header("origin", ORIGIN)
        .header("content-type", format!("multipart/form-data; boundary={b}"))
        .body(Body::from(body))
        .unwrap()
}

fn get(uri: &str, key: Option<&str>) -> Request<Body> {
    let mut b = Request::builder().method("GET").uri(uri).header("x-forwarded-for", "198.51.100.1");
    if let Some(k) = key {
        b = b.header("authorization", format!("Bearer {k}"));
    }
    b.body(Body::empty()).unwrap()
}

async fn body_bytes(resp: axum::response::Response) -> Vec<u8> {
    resp.into_body().collect().await.unwrap().to_bytes().to_vec()
}

async fn body_json(resp: axum::response::Response) -> Value {
    serde_json::from_slice(&body_bytes(resp).await).unwrap()
}

#[tokio::test]
async fn widget_script_is_served_cross_origin() {
    let p = temp_path("widget");
    let (app, _, _) = setup(&p).await;
    let resp = app.clone().oneshot(get("/feedback.js", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let h = resp.headers();
    assert!(h["content-type"].to_str().unwrap().starts_with("text/javascript"));
    assert_eq!(h["cross-origin-resource-policy"], "cross-origin");
    let js = String::from_utf8(body_bytes(resp).await).unwrap();
    assert!(js.contains("/api/feedback"));
    // Cloudflare's WAF 403s any asset containing a literal `<script`.
    assert!(!js.contains("<script"));

    let resp = app.clone().oneshot(get("/feedback/html2canvas-v2.js", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers()["cache-control"].to_str().unwrap().contains("immutable"));
    let lib = String::from_utf8(body_bytes(resp).await).unwrap();
    assert!(lib.contains("html2canvas-pro"), "screenshot lib must be html2canvas-pro (parses oklch)");
    assert!(!lib.contains("<script"));

    // Older widget copies still ask for v1: same build, short cache.
    let resp = app.oneshot(get("/feedback/html2canvas-v1.js", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(!resp.headers()["cache-control"].to_str().unwrap().contains("immutable"));
    cleanup(&p);
}

#[tokio::test]
async fn json_ingest_lands_in_admin_list() {
    let p = temp_path("json");
    let (app, site_id, key) = setup(&p).await;

    // Unknown / missing key looks like a missing route: empty 404, no CORS.
    let resp = app
        .clone()
        .oneshot(json_post("203.0.113.10", serde_json::json!({ "website": "nope", "message": "hi" })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(resp.headers().get("access-control-allow-origin").is_none());
    assert!(body_bytes(resp).await.is_empty());

    let resp = app
        .clone()
        .oneshot(json_post(
            "203.0.113.10",
            serde_json::json!({
                "website": key, "message": "  The export button is broken  ",
                "url": "https://shop.example.com/cart", "appVersion": "1.2.3",
                "endUserEmail": "ada@example.com",
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["access-control-allow-origin"], ORIGIN);
    let created = body_json(resp).await;
    assert_eq!(created["ok"], true);

    // Admin list: no key → 404; read key works for GET.
    let resp = app.clone().oneshot(get("/api/stats/feedback", None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = app
        .clone()
        .oneshot(get(&format!("/api/stats/feedback?site={site_id}"), Some(READ_KEY)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let list = body_json(resp).await;
    let row = &list["rows"][0];
    assert_eq!(row["message"], "The export button is broken");
    assert_eq!(row["site_name"], "Shop");
    assert_eq!(row["status"], "new");
    assert_eq!(row["app_version"], "1.2.3");
    assert_eq!(row["user_agent"], "Mozilla/5.0 Test");
    assert_eq!(row["has_screenshot"], false);
    assert_eq!(list["counts"]["total"], 1);
    assert_eq!(list["counts"]["new"], 1);

    // Another site's filter sees nothing.
    let resp = app
        .oneshot(get("/api/stats/feedback?site=other", Some(ADMIN_KEY)))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["counts"]["total"], 0);
    cleanup(&p);
}

#[tokio::test]
async fn oversized_message_is_413_and_empty_is_400() {
    let p = temp_path("msg");
    let (app, _, key) = setup(&p).await;
    // 5000 UTF-16 units is the limit; one emoji is 2 units.
    let at_limit = "a".repeat(MAX_MESSAGE_CHARS);
    let resp = app
        .clone()
        .oneshot(json_post("203.0.113.20", serde_json::json!({ "website": key, "message": at_limit })))
        .await
        .unwrap();
    // 5000 ASCII chars fits in the JSON route body cap and is accepted.
    assert_eq!(resp.status(), StatusCode::OK);

    let over = format!("{}😀", "a".repeat(MAX_MESSAGE_CHARS - 1));
    let resp = app
        .clone()
        .oneshot(json_post("203.0.113.20", serde_json::json!({ "website": key, "message": over })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(resp.headers()["access-control-allow-origin"], ORIGIN);
    assert_eq!(body_json(resp).await["error"], "message_too_long");

    let resp = app
        .oneshot(json_post("203.0.113.20", serde_json::json!({ "website": key, "message": "   " })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["error"], "message_required");
    cleanup(&p);
}

#[tokio::test]
async fn multipart_screenshot_roundtrip_and_limits() {
    let p = temp_path("shot");
    let (app, _, key) = setup(&p).await;
    let png: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR-not-a-real-image\r\n--";

    let resp = app
        .clone()
        .oneshot(multipart_post(
            "203.0.113.30",
            &[("website", &key), ("message", "see screenshot")],
            Some(("image/png", png)),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let id = body_json(resp).await["id"].as_i64().unwrap();

    let resp = app
        .clone()
        .oneshot(get("/api/stats/feedback", Some(ADMIN_KEY)))
        .await
        .unwrap();
    let row = body_json(resp).await["rows"][0].clone();
    assert_eq!(row["has_screenshot"], true);
    assert_eq!(row["screenshot_bytes"], png.len());

    let resp = app
        .clone()
        .oneshot(get(&format!("/api/feedback/{id}/screenshot"), Some(READ_KEY)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "image/png");
    assert_eq!(resp.headers()["x-content-type-options"], "nosniff");
    assert_eq!(body_bytes(resp).await, png);
    let resp = app
        .clone()
        .oneshot(get(&format!("/api/feedback/{id}/screenshot"), None))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // Wrong type → 415.
    let resp = app
        .clone()
        .oneshot(multipart_post(
            "203.0.113.30",
            &[("website", &key), ("message", "gif")],
            Some(("image/gif", b"GIF89a")),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);

    // One byte over 2 MiB → the handler's JSON 413.
    let big = vec![0u8; MAX_SCREENSHOT_BYTES + 1];
    let resp = app
        .clone()
        .oneshot(multipart_post(
            "203.0.113.31",
            &[("website", &key), ("message", "big")],
            Some(("image/jpeg", &big)),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(body_json(resp).await["error"], "screenshot_too_large");

    // Far over the route cap → axum's body limit, still 413.
    let huge = vec![0u8; 4 * 1024 * 1024];
    let resp = app
        .clone()
        .oneshot(multipart_post(
            "203.0.113.32",
            &[("website", &key), ("message", "huge")],
            Some(("image/jpeg", &huge)),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);

    // Malformed multipart → 400.
    let req = Request::builder()
        .method("POST")
        .uri("/api/feedback")
        .header("x-forwarded-for", "203.0.113.33")
        .header("content-type", "multipart/form-data; boundary=zz")
        .body(Body::from("garbage"))
        .unwrap();
    assert_eq!(app.oneshot(req).await.unwrap().status(), StatusCode::BAD_REQUEST);
    cleanup(&p);
}

#[tokio::test]
async fn status_triage_is_admin_only() {
    let p = temp_path("status");
    let (app, _, key) = setup(&p).await;
    let resp = app
        .clone()
        .oneshot(json_post("203.0.113.40", serde_json::json!({ "website": key, "message": "triage me" })))
        .await
        .unwrap();
    let id = body_json(resp).await["id"].as_i64().unwrap();

    let patch = |k: &str, body: &str| {
        Request::builder()
            .method("POST")
            .uri(format!("/api/feedback/{id}"))
            .header("x-forwarded-for", "198.51.100.2")
            .header("authorization", format!("Bearer {k}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    // The read key never unlocks a write.
    let resp = app.clone().oneshot(patch(READ_KEY, r#"{"status":"read"}"#)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = app.clone().oneshot(patch(ADMIN_KEY, r#"{"status":"bogus"}"#)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let resp = app.clone().oneshot(patch(ADMIN_KEY, r#"{"status":"archived"}"#)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["status"], "archived");

    let resp = app
        .clone()
        .oneshot(get("/api/stats/feedback?status=new", Some(ADMIN_KEY)))
        .await
        .unwrap();
    let list = body_json(resp).await;
    assert_eq!(list["rows"].as_array().unwrap().len(), 0);
    assert_eq!(list["counts"]["archived"], 1);
    let resp = app
        .oneshot(get("/api/stats/feedback?status=bogus", Some(ADMIN_KEY)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    cleanup(&p);
}

#[tokio::test]
async fn ingest_is_rate_limited_per_ip() {
    let p = temp_path("rate");
    let (app, _, key) = setup(&p).await;
    for _ in 0..20 {
        let resp = app
            .clone()
            .oneshot(json_post("203.0.113.50", serde_json::json!({ "website": key, "message": "x" })))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    let resp = app
        .clone()
        .oneshot(json_post("203.0.113.50", serde_json::json!({ "website": key, "message": "x" })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS);
    // Another IP is unaffected.
    let resp = app
        .oneshot(json_post("203.0.113.51", serde_json::json!({ "website": key, "message": "x" })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    cleanup(&p);
}

#[tokio::test]
async fn preflight_reflects_origin() {
    let p = temp_path("options");
    let (app, _, _) = setup(&p).await;
    let req = Request::builder()
        .method("OPTIONS")
        .uri("/api/feedback")
        .header("origin", ORIGIN)
        .header("x-forwarded-for", "203.0.113.60")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    assert_eq!(resp.headers()["access-control-allow-origin"], ORIGIN);
    cleanup(&p);
}

fn admin_post(uri: &str, key: Option<&str>, body: Value) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(uri)
        .header("x-forwarded-for", "198.51.100.9")
        .header("content-type", "application/json");
    if let Some(k) = key {
        b = b.header("authorization", format!("Bearer {k}"));
    }
    b.body(Body::from(body.to_string())).unwrap()
}

#[tokio::test]
async fn import_dry_runs_then_writes_once_with_original_fields() {
    let p = temp_path("import");
    let (app, site_id, _) = setup(&p).await;
    // "/9j/" is base64 for the JPEG SOI marker ff d8 ff.
    let body = serde_json::json!({
        "site": site_id,
        "rows": [
            { "message": "old one", "created_at": 1777364909000i64, "url": "https://shop.example.com/a",
              "end_user_id": "u1", "end_user_name": "Ann", "end_user_email": "ann@example.com",
              "app_version": "0.11.0", "user_agent": "Mozilla/5.0 Old" },
            { "message": "with shot", "created_at": 1777364950000i64, "status": "read",
              "screenshot": "data:image/jpeg;base64,/9j/" }
        ]
    });

    // Admin only: no key and the read key both look like a missing route.
    let resp = app.clone().oneshot(admin_post("/api/import/feedback", None, body.clone())).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = app.clone().oneshot(admin_post("/api/import/feedback", Some(READ_KEY), body.clone())).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let mut dry = body.clone();
    dry["dry_run"] = Value::Bool(true);
    let resp = app.clone().oneshot(admin_post("/api/import/feedback", Some(ADMIN_KEY), dry.clone())).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let plan = body_json(resp).await;
    assert_eq!(plan["imported"], 2);
    assert_eq!(plan["rows"][1]["screenshot_bytes"], 3);
    let list = body_json(app.clone().oneshot(get("/api/stats/feedback?all=1", Some(ADMIN_KEY))).await.unwrap()).await;
    assert_eq!(list["counts"]["total"], 0, "dry run must not write");

    let resp = app.clone().oneshot(admin_post("/api/import/feedback", Some(ADMIN_KEY), body.clone())).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let done = body_json(resp).await;
    assert_eq!(done["imported"], 2);
    let shot_id = done["rows"][1]["id"].as_i64().unwrap();

    let list = body_json(app.clone().oneshot(get("/api/stats/feedback?all=1", Some(ADMIN_KEY))).await.unwrap()).await;
    assert_eq!(list["counts"]["total"], 2);
    assert_eq!(list["counts"]["read"], 1);
    let old = list["rows"].as_array().unwrap().iter().find(|r| r["message"] == "old one").unwrap().clone();
    assert_eq!(old["created_at"], 1777364909000i64);
    assert_eq!(old["end_user_email"], "ann@example.com");
    assert_eq!(old["user_agent"], "Mozilla/5.0 Old");
    assert_eq!(old["country"], Value::Null);

    let resp = app.clone().oneshot(get(&format!("/api/feedback/{shot_id}/screenshot"), Some(ADMIN_KEY))).await.unwrap();
    assert_eq!(resp.headers()["content-type"], "image/jpeg");
    assert_eq!(body_bytes(resp).await, [0xff, 0xd8, 0xff]);

    // Re-run is a no-op.
    let again = body_json(app.clone().oneshot(admin_post("/api/import/feedback", Some(ADMIN_KEY), body)).await.unwrap()).await;
    assert_eq!(again["imported"], 0);
    assert_eq!(again["skipped"], 2);
    cleanup(&p);
}

#[tokio::test]
async fn import_rejects_the_whole_batch_on_one_bad_row() {
    let p = temp_path("import_bad");
    let (app, site_id, _) = setup(&p).await;
    for (row, code) in [
        (serde_json::json!({ "message": "x" }), "invalid_created_at"),
        (serde_json::json!({ "message": " ", "created_at": 1 }), "message_required"),
        (serde_json::json!({ "message": "x", "created_at": 1, "status": "done" }), "invalid_status"),
        (serde_json::json!({ "message": "x", "created_at": 1, "screenshot": "data:image/gif;base64,R0lG" }), "screenshot_type"),
        (serde_json::json!({ "message": "x", "created_at": 1, "screenshot": "not a data url" }), "invalid_screenshot"),
    ] {
        let body = serde_json::json!({ "site": site_id, "rows": [ { "message": "fine", "created_at": 5 }, row ] });
        let resp = app.clone().oneshot(admin_post("/api/import/feedback", Some(ADMIN_KEY), body)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let j = body_json(resp).await;
        assert_eq!(j["error"], code);
        assert_eq!(j["index"], 1);
    }
    let list = body_json(app.clone().oneshot(get("/api/stats/feedback?all=1", Some(ADMIN_KEY))).await.unwrap()).await;
    assert_eq!(list["counts"]["total"], 0);
    let resp = app
        .oneshot(admin_post("/api/import/feedback", Some(ADMIN_KEY), serde_json::json!({ "site": "nope", "rows": [] })))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    cleanup(&p);
}
