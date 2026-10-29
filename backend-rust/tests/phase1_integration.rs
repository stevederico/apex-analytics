//! Integration tests for the Phase 1 endpoints:
//! batch ingest (+ gzip), error-issue lifecycle + PATCH, lifecycle buckets,
//! attribution, and breakdown — each driven through the full `build_app` router
//! via `tower::ServiceExt::oneshot`, same harness style as routes_integration.rs.

use std::collections::HashSet;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use apex_analytics::app::{build_app, AppState, VersionConfig};
use apex_analytics::db::Db;
use flate2::write::GzEncoder;
use flate2::Compression;
use tower::ServiceExt; // for `oneshot`

const ADMIN_KEY: &str = "test-admin-key-0000000000";

const LANDING_ALLOWED_ORIGIN: &str = "https://www.example.com";

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_db_path() -> String {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!("dottie_axum_p1_{}_{}.db", std::process::id(), n));
    let p = path.to_string_lossy().into_owned();
    for f in [p.clone(), format!("{p}-wal"), format!("{p}-shm")] {
        let _ = std::fs::remove_file(&f);
    }
    p
}

fn cleanup(p: &str) {
    for f in [p.to_string(), format!("{p}-wal"), format!("{p}-shm")] {
        let _ = std::fs::remove_file(&f);
    }
}

async fn fresh() -> (Db, String) {
    let p = temp_db_path();
    let db = Db::connect(&p, HashSet::new(), HashSet::new())
        .await
        .expect("connect db");
    seed_desktop_user(&db).await;
    (db, p)
}

/// `fresh` without the seeded desktop user, for exact user-count asserts.
async fn fresh_unseeded() -> (Db, String) {
    let p = temp_db_path();
    let db = Db::connect(&p, HashSet::new(), HashSet::new())
        .await
        .expect("connect db");
    (db, p)
}

fn state_for(db: Db) -> AppState {
    AppState::new(
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
    .with_landing_origin(LANDING_ALLOWED_ORIGIN.to_string())
}

fn req_post(uri: &str, body: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut b = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

fn req_get(uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut b = Request::builder().method("GET").uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Body::empty()).unwrap()
}

fn req_patch(uri: &str, body: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut b = Request::builder()
        .method("PATCH")
        .uri(uri)
        .header("content-type", "application/json");
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Body::from(body.to_string())).unwrap()
}

const DESKTOP_TOKEN: &str = "test-desktop-token-00000000";
const DESKTOP_HDR: (&str, &str) = ("authorization", "Bearer test-desktop-token-00000000");
/// Seed the desktop bearer user that `DESKTOP_HDR` authenticates as. The
/// shared `X-Dottie-Client` key is retired, so ingest needs a real user token.
async fn seed_desktop_user(db: &Db) {
    sqlx::query(
        "INSERT INTO users (name, email, install_id, token, created_at, updated_at)
         VALUES ('Desk', 'desk@example.com', 'seed-desktop', ?, 0, 0)",
    )
    .bind(DESKTOP_TOKEN)
    .execute(db.pool())
    .await
    .expect("seed desktop user");
}

fn admin_hdr() -> (&'static str, String) {
    ("authorization", format!("Bearer {ADMIN_KEY}"))
}

async fn body_json(resp: Response<Body>) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    if bytes.is_empty() {
        return serde_json::Value::Null;
    }
    serde_json::from_slice(&bytes).expect("response body is JSON")
}

/// Drive an admin GET and return its JSON body (asserts 200).
async fn admin_get(app: &axum::Router, uri: &str) -> serde_json::Value {
    let (k, v) = admin_hdr();
    let resp = app
        .clone()
        .oneshot(req_get(uri, &[(k, v.as_str())]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "GET {uri}");
    body_json(resp).await
}

// ============================================================================
// batch ingest
// ============================================================================

#[tokio::test]
async fn batch_accepts_and_dedupes() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    // 3 items: two share an event_id (deduped to one), one plain. All valid.
    let batch = r#"{"events":[
        {"install_id":"i1","name":"chat-sent","event_id":"e1"},
        {"install_id":"i1","name":"chat-sent","event_id":"e1"},
        {"install_id":"i1","name":"app-open"}
    ]}"#;
    let resp = app
        .clone()
        .oneshot(req_post("/api/events/batch", batch, &[DESKTOP_HDR]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let j = body_json(resp).await;
    assert_eq!(j["ok"], serde_json::json!(true));
    assert_eq!(j["received"], serde_json::json!(3));
    // record_event runs 3 times (accepted counts calls that didn't error), but
    // the DB deduped the repeated event_id → 2 rows.
    let stats = admin_get(&app, "/api/stats").await;
    assert_eq!(stats["events_24h"], serde_json::json!(2));
    cleanup(&p);
}

#[tokio::test]
async fn batch_rejects_bad_shape_and_oversize() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    // Not an array / no events key.
    let r1 = app
        .clone()
        .oneshot(req_post("/api/events/batch", r#"{"nope":1}"#, &[DESKTOP_HDR]))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(r1).await["error"], serde_json::json!("invalid_batch"));
    // Over 100 events.
    let many: Vec<String> = (0..101)
        .map(|i| format!(r#"{{"install_id":"i{i}","name":"x"}}"#))
        .collect();
    let big = format!(r#"{{"events":[{}]}}"#, many.join(","));
    let r2 = app
        .clone()
        .oneshot(req_post("/api/events/batch", &big, &[DESKTOP_HDR]))
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(r2).await["error"], serde_json::json!("batch_too_large"));
    cleanup(&p);
}

#[tokio::test]
async fn batch_404_without_desktop_bearer() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(req_post(
            "/api/events/batch",
            r#"{"events":[{"install_id":"i1","name":"x"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    cleanup(&p);
}

#[tokio::test]
async fn batch_accepts_gzip() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let json = r#"{"events":[{"install_id":"gz1","name":"chat-sent"},{"install_id":"gz1","name":"app-open"}]}"#;
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(json.as_bytes()).unwrap();
    let gz = enc.finish().unwrap();
    let req = Request::builder()
        .method("POST")
        .uri("/api/events/batch")
        .header("content-type", "application/json")
        .header("content-encoding", "gzip")
        .header(DESKTOP_HDR.0, DESKTOP_HDR.1)
        .body(Body::from(gz))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["accepted"], serde_json::json!(2));
    let stats = admin_get(&app, "/api/stats").await;
    assert_eq!(stats["events_24h"], serde_json::json!(2));
    cleanup(&p);
}

// ============================================================================
// error issues + PATCH lifecycle
// ============================================================================

#[tokio::test]
async fn issues_lifecycle_via_http() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let fire_error = |app: axum::Router| async move {
        app.oneshot(req_post(
            "/api/events",
            r#"{"install_id":"i1","name":"error.thrown","props":{"file":"a.rs","function":"f","line":9}}"#,
            &[DESKTOP_HDR],
        ))
        .await
        .unwrap()
    };
    // First error → one active issue.
    assert_eq!(fire_error(app.clone()).await.status(), StatusCode::OK);
    let issues = admin_get(&app, "/api/stats/issues").await;
    assert_eq!(issues["rows"].as_array().unwrap().len(), 1);
    assert_eq!(issues["rows"][0]["status"], serde_json::json!("active"));
    let id = issues["rows"][0]["id"].as_i64().unwrap();

    // PATCH resolve.
    let (k, v) = admin_hdr();
    let resp = app
        .clone()
        .oneshot(req_patch(
            &format!("/api/issues/{id}"),
            r#"{"status":"resolved"}"#,
            &[(k, v.as_str())],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["status"], serde_json::json!("resolved"));

    // Filter by status.
    let resolved = admin_get(&app, "/api/stats/issues?status=resolved").await;
    assert_eq!(resolved["rows"].as_array().unwrap().len(), 1);

    // Resolving without a version defaults to the configured latest release
    // ("9.99" in this fixture) instead of NULL — NULL meant "reopen on any
    // sighting", which let stale builds flap issues back to regressed.
    let resolved_row = &resolved["rows"][0];
    assert_eq!(resolved_row["resolved_in_version"], serde_json::json!("9.99"));

    // Same error recurs from a build at/after the fix → regressed.
    let recur = app
        .clone()
        .oneshot(req_post(
            "/api/events",
            r#"{"install_id":"i1","name":"error.thrown","version":"9.99","props":{"file":"a.rs","function":"f","line":9}}"#,
            &[DESKTOP_HDR],
        ))
        .await
        .unwrap();
    assert_eq!(recur.status(), StatusCode::OK);
    let regressed = admin_get(&app, "/api/stats/issues?status=regressed").await;
    assert_eq!(regressed["rows"].as_array().unwrap().len(), 1);
    assert_eq!(regressed["rows"][0]["count"], serde_json::json!(2));
    cleanup(&p);
}

// C2) Resolving with a fix version keeps stale builds from reopening the issue.
#[tokio::test]
async fn resolve_with_version_ignores_older_builds() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let fire = |app: axum::Router, version: &str| {
        let body = format!(
            r#"{{"install_id":"i1","name":"error.thrown","version":"{version}","props":{{"file":"a.rs","function":"f","line":9}}}}"#
        );
        async move {
            app.oneshot(req_post("/api/events", &body, &[DESKTOP_HDR]))
                .await
                .unwrap()
        }
    };
    assert_eq!(fire(app.clone(), "2026.8.1.7").await.status(), StatusCode::OK);
    let id = admin_get(&app, "/api/stats/issues").await["rows"][0]["id"]
        .as_i64()
        .unwrap();

    let (k, v) = admin_hdr();
    let resp = app
        .clone()
        .oneshot(req_patch(
            &format!("/api/issues/{id}"),
            r#"{"status":"resolved","version":"2026.8.5"}"#,
            &[(k, v.as_str())],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(
        body_json(resp).await["resolved_in_version"],
        serde_json::json!("2026.8.5")
    );

    // Old build in the field: counted, still resolved.
    assert_eq!(fire(app.clone(), "2026.8.1.7").await.status(), StatusCode::OK);
    let resolved = admin_get(&app, "/api/stats/issues?status=resolved").await;
    assert_eq!(resolved["rows"].as_array().unwrap().len(), 1);
    assert_eq!(resolved["rows"][0]["count"], serde_json::json!(2));

    // Build carrying the fix: real regression.
    assert_eq!(fire(app.clone(), "2026.8.8").await.status(), StatusCode::OK);
    let regressed = admin_get(&app, "/api/stats/issues?status=regressed").await;
    assert_eq!(regressed["rows"].as_array().unwrap().len(), 1);
    cleanup(&p);
}

#[tokio::test]
async fn patch_issue_bad_status_and_missing() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let (k, v) = admin_hdr();
    // Bad status value → 400.
    let r1 = app
        .clone()
        .oneshot(req_patch("/api/issues/1", r#"{"status":"nope"}"#, &[(k, v.as_str())]))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::BAD_REQUEST);
    // Valid status but unknown id → 404.
    let r2 = app
        .clone()
        .oneshot(req_patch("/api/issues/999", r#"{"status":"active"}"#, &[(k, v.as_str())]))
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::NOT_FOUND);
    // Admin guard: no key → 404.
    let r3 = app
        .oneshot(req_patch("/api/issues/1", r#"{"status":"active"}"#, &[]))
        .await
        .unwrap();
    assert_eq!(r3.status(), StatusCode::NOT_FOUND);
    cleanup(&p);
}

// ============================================================================
// lifecycle / attribution / breakdown
// ============================================================================

#[tokio::test]
async fn lifecycle_shape() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let j = admin_get(&app, "/api/stats/lifecycle?weeks=4").await;
    let weeks = j["weeks"].as_array().unwrap();
    assert_eq!(weeks.len(), 4);
    for w in weeks {
        for key in ["new", "returning", "resurrecting", "dormant", "week"] {
            assert!(w.get(key).is_some(), "missing {key}");
        }
    }
    cleanup(&p);
}

#[tokio::test]
async fn attribution_joins_utm() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    // A landing visit carrying UTM, keyed by iid.
    let lv = app
        .clone()
        .oneshot(req_post(
            "/api/landing-visit",
            r#"{"iid":"land-1","utm_source":"newsletter","utm_medium":"email","utm_campaign":"launch"}"#,
            &[("origin", LANDING_ALLOWED_ORIGIN)],
        ))
        .await
        .unwrap();
    assert_eq!(lv.status(), StatusCode::OK);
    // Register forwarding that iid → maps the install to the landing visit.
    let reg = app
        .clone()
        .oneshot(req_post(
            "/api/register",
            r#"{"name":"Attr User","email":"attr@example.com","install_id":"attr-install","iid":"land-1"}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(reg.status(), StatusCode::OK);
    // Attribution groups the signup under the newsletter campaign.
    let j = admin_get(&app, "/api/stats/attribution").await;
    assert_eq!(j["total"], serde_json::json!(1));
    let row = &j["rows"][0];
    assert_eq!(row["source"], serde_json::json!("newsletter"));
    assert_eq!(row["campaign"], serde_json::json!("launch"));
    assert_eq!(row["signups"], serde_json::json!(1));
    cleanup(&p);
}

#[tokio::test]
async fn users_list() {
    let (db, p) = fresh_unseeded().await;
    let app = build_app(state_for(db));
    // Register two users; fire a chat-sent + a ping for one of them, on its
    // own bearer (no seeded desktop user, so the totals stay exact).
    let mut tokens = Vec::new();
    for (email, iid) in [("u1@x.com", "ui-1"), ("u2@x.com", "ui-2")] {
        let body = format!(
            r#"{{"name":"Real User","email":"{email}","install_id":"{iid}"}}"#
        );
        let r = app.clone().oneshot(req_post("/api/register", &body, &[])).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        tokens.push(body_json(r).await["token"].as_str().unwrap().to_string());
    }
    let u1 = format!("Bearer {}", tokens[0]);
    let r = app
        .clone()
        .oneshot(req_post("/api/events", r#"{"install_id":"ui-1","name":"chat-sent"}"#, &[("authorization", &u1)]))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    let r = app
        .clone()
        .oneshot(req_post("/api/ping", r#"{"install_id":"ui-1"}"#, &[("authorization", &u1)]))
        .await
        .unwrap();
    assert_eq!(r.status(), StatusCode::OK);

    let j = admin_get(&app, "/api/stats/users?limit=10").await;
    assert_eq!(j["total"], serde_json::json!(2));
    let rows = j["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    // ui-1 pinged → sorts first (non-null last_seen); has 1 msg, 1 active day.
    assert_eq!(rows[0]["email"], serde_json::json!("u1@x.com"));
    assert_eq!(rows[0]["msgs"], serde_json::json!(1));
    assert_eq!(rows[0]["active_days"], serde_json::json!(1));
    // ui-2 never pinged → last_seen null, sorts last.
    assert_eq!(rows[1]["last_seen"], serde_json::Value::Null);
    cleanup(&p);
}

#[tokio::test]
async fn breakdown_by_version_and_invalid() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    // Two users on 9.98, one on 9.99.
    for (email, ver) in [("a@x.com", "9.98"), ("b@x.com", "9.98"), ("c@x.com", "9.99")] {
        let body = format!(
            r#"{{"name":"Test User","email":"{email}","install_id":"{email}","version":"{ver}"}}"#
        );
        let r = app.clone().oneshot(req_post("/api/register", &body, &[])).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }
    let j = admin_get(&app, "/api/stats/breakdown?by=version").await;
    assert_eq!(j["by"], serde_json::json!("version"));
    let rows = j["rows"].as_array().unwrap();
    // Ordered by users desc → 9.98 (2) first.
    assert_eq!(rows[0]["bucket"], serde_json::json!("9.98"));
    assert_eq!(rows[0]["users"], serde_json::json!(2));

    // Invalid dimension → 400.
    let (k, v) = admin_hdr();
    let bad = app
        .oneshot(req_get("/api/stats/breakdown?by=email", &[(k, v.as_str())]))
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    cleanup(&p);
}

#[tokio::test]
async fn landing_event_ingest_origin_and_reports() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));

    // Wrong Origin → 403 (probe/CORS parity with landing-visit).
    let bad_origin = app
        .clone()
        .oneshot(req_post(
            "/api/landing-event",
            r#"{"iid":"le-1","name":"download"}"#,
            &[("origin", "https://evil.example")],
        ))
        .await
        .unwrap();
    assert_eq!(bad_origin.status(), StatusCode::FORBIDDEN);

    // Missing name → 400.
    let no_name = app
        .clone()
        .oneshot(req_post(
            "/api/landing-event",
            r#"{"iid":"le-1"}"#,
            &[("origin", LANDING_ALLOWED_ORIGIN)],
        ))
        .await
        .unwrap();
    assert_eq!(no_name.status(), StatusCode::BAD_REQUEST);

    // Two download events (same iid) + one scroll-depth event.
    for body in [
        r#"{"iid":"le-1","name":"download","props":{"location":"hero"}}"#,
        r#"{"iid":"le-1","name":"download","props":{"location":"footer"}}"#,
        r#"{"iid":"le-2","name":"scroll-depth","props":{"depth":50}}"#,
    ] {
        let r = app
            .clone()
            .oneshot(req_post(
                "/api/landing-event",
                body,
                &[("origin", LANDING_ALLOWED_ORIGIN)],
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    // Breakdown: download has 2 events across 1 visitor; scroll-depth 1/1.
    let ev = admin_get(&app, "/api/stats/landing-events").await;
    assert_eq!(ev["total"], serde_json::json!(3));
    let rows = ev["rows"].as_array().unwrap();
    assert_eq!(rows[0]["name"], serde_json::json!("download"));
    assert_eq!(rows[0]["count"], serde_json::json!(2));
    assert_eq!(rows[0]["visitors"], serde_json::json!(1));

    // Funnel: le-1 visited + downloaded; register le-1 → installed + signed_up.
    app.clone()
        .oneshot(req_post(
            "/api/landing-visit",
            r#"{"iid":"le-1"}"#,
            &[("origin", LANDING_ALLOWED_ORIGIN)],
        ))
        .await
        .unwrap();
    app.clone()
        .oneshot(req_post(
            "/api/register",
            r#"{"name":"Funnel User","email":"funnel@example.com","install_id":"fun-install","iid":"le-1"}"#,
            &[],
        ))
        .await
        .unwrap();
    let f = admin_get(&app, "/api/stats/landing-funnel").await;
    let steps = f["steps"].as_array().unwrap();
    assert_eq!(steps[0]["step"], serde_json::json!("visited"));
    assert_eq!(steps[0]["count"], serde_json::json!(1));
    assert_eq!(steps[1]["step"], serde_json::json!("downloaded"));
    assert_eq!(steps[1]["count"], serde_json::json!(1));
    assert_eq!(steps[3]["step"], serde_json::json!("signed_up"));
    assert_eq!(steps[3]["count"], serde_json::json!(1));
    cleanup(&p);
}

#[tokio::test]
async fn delete_landing_purges_iid() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    // Seed a visit + two events for one iid.
    app.clone()
        .oneshot(req_post(
            "/api/landing-visit",
            r#"{"iid":"purge-1"}"#,
            &[("origin", LANDING_ALLOWED_ORIGIN)],
        ))
        .await
        .unwrap();
    for name in ["download", "scroll-depth"] {
        let body = format!(r#"{{"iid":"purge-1","name":"{name}"}}"#);
        app.clone()
            .oneshot(req_post(
                "/api/landing-event",
                &body,
                &[("origin", LANDING_ALLOWED_ORIGIN)],
            ))
            .await
            .unwrap();
    }
    // Purge via the POST alias (admin-gated). Reports the counts.
    let (k, v) = admin_hdr();
    let del = app
        .clone()
        .oneshot(req_post("/api/landing/purge-1", "", &[(k, v.as_str())]))
        .await
        .unwrap();
    assert_eq!(del.status(), StatusCode::OK);
    let dj = body_json(del).await;
    assert_eq!(dj["events_deleted"], serde_json::json!(2));
    assert_eq!(dj["visits_deleted"], serde_json::json!(1));
    // Report now empty for that window.
    let ev = admin_get(&app, "/api/stats/landing-events?hours=1").await;
    assert_eq!(ev["total"], serde_json::json!(0));
    // Purge is admin-gated: no key → 404 (probe-resistant).
    let unauth = app
        .oneshot(req_post("/api/landing/purge-1", "", &[]))
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::NOT_FOUND);
    cleanup(&p);
}

#[tokio::test]
async fn site_import_and_reports() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let (k, v) = admin_hdr();

    // Import: 3 pageviews (2 distinct visitors) + 2 custom events, explicit ts.
    // Enrichment fields (browser/os/device/country) exercise breakdown reports.
    let body = r#"{
      "site": { "id": "site-abc", "name": "Acme", "domain": "acme.test" },
      "events": [
        { "type": 1, "url": "/?utm=1", "session_id": "s1", "distinct_id": "v1", "referrer": "google.com", "browser": "Chrome", "os": "macOS", "device": "desktop", "country": "US", "created_at": 1783000001000 },
        { "type": 1, "url": "/pricing", "session_id": "s1", "distinct_id": "v1", "browser": "Chrome", "os": "macOS", "device": "desktop", "country": "US", "created_at": 1783000002000 },
        { "type": 1, "url": "/", "session_id": "s2", "distinct_id": "v2", "referrer": "google.com", "browser": "Safari", "os": "iOS", "device": "mobile", "country": "CA", "created_at": 1783000003000 },
        { "type": 2, "name": "signup-click", "session_id": "s2", "distinct_id": "v2", "props": {"plan":"pro"}, "browser": "Safari", "os": "iOS", "device": "mobile", "country": "CA", "created_at": 1783000004000 },
        { "type": 2, "name": "signup-click", "session_id": "s1", "distinct_id": "v1", "browser": "Chrome", "os": "macOS", "device": "desktop", "country": "US", "created_at": 1783000005000 }
      ]
    }"#;
    let imp = app
        .clone()
        .oneshot(req_post("/api/import/site-events", body, &[(k, v.as_str())]))
        .await
        .unwrap();
    assert_eq!(imp.status(), StatusCode::OK);
    assert_eq!(body_json(imp).await["imported"], serde_json::json!(5));

    // Same batch again is a no-op: natural key matches, nothing new is stored.
    let again = app
        .clone()
        .oneshot(req_post("/api/import/site-events", body, &[(k, v.as_str())]))
        .await
        .unwrap();
    assert_eq!(again.status(), StatusCode::OK);
    let again_json = body_json(again).await;
    assert_eq!(again_json["imported"], serde_json::json!(0));
    assert_eq!(again_json["skipped"], serde_json::json!(5));

    // Import is admin-gated: no key → 404.
    let unauth = app
        .clone()
        .oneshot(req_post("/api/import/site-events", body, &[]))
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::NOT_FOUND);

    // /api/sites lists the site with lifetime counts (5 events, 2 visitors).
    let sites = admin_get(&app, "/api/sites").await;
    let s0 = &sites["sites"][0];
    assert_eq!(s0["id"], serde_json::json!("site-abc"));
    assert_eq!(s0["events"], serde_json::json!(5));
    assert_eq!(s0["visitors"], serde_json::json!(2));

    // Event breakdown: signup-click ×2 across 2 visitors.
    let ev = admin_get(&app, "/api/site/events?site=site-abc&hours=8760").await;
    assert_eq!(ev["rows"][0]["name"], serde_json::json!("signup-click"));
    assert_eq!(ev["rows"][0]["count"], serde_json::json!(2));
    assert_eq!(ev["rows"][0]["visitors"], serde_json::json!(2));

    // Top pages (path): "/" has 2 views (/?utm=1 + /), /pricing has 1.
    let pages = admin_get(&app, "/api/site/pages?site=site-abc&kind=path&hours=8760").await;
    assert_eq!(pages["rows"][0]["url"], serde_json::json!("/"));
    assert_eq!(pages["rows"][0]["views"], serde_json::json!(2));

    // Entry pages: s1 starts at /?utm=1 → path /; s2 starts at /.
    let entry = admin_get(&app, "/api/site/pages?site=site-abc&kind=entry&hours=8760").await;
    assert_eq!(entry["rows"][0]["url"], serde_json::json!("/"));
    assert_eq!(entry["rows"][0]["views"], serde_json::json!(2));

    // Exit: s1 ends /pricing; s2 ends /.
    let exit = admin_get(&app, "/api/site/pages?site=site-abc&kind=exit&hours=8760").await;
    assert!(exit["rows"].as_array().unwrap().len() >= 2);

    // Referrers: google.com, 2 visitors.
    let refs = admin_get(&app, "/api/site/referrers?site=site-abc&hours=8760").await;
    assert_eq!(refs["rows"][0]["referrer"], serde_json::json!("google.com"));
    assert_eq!(refs["rows"][0]["visitors"], serde_json::json!(2));

    // Breakdown by browser — one row per distinct browser (no duplicate names).
    let br = admin_get(&app, "/api/site/breakdown?site=site-abc&by=browser&hours=8760").await;
    assert_eq!(br["by"], serde_json::json!("browser"));
    let brows = br["rows"].as_array().unwrap();
    assert_eq!(brows.len(), 2); // Chrome + Safari
    let names: Vec<&str> = brows.iter().filter_map(|r| r["name"].as_str()).collect();
    assert!(names.contains(&"Chrome"));
    assert!(names.contains(&"Safari"));

    // Country breakdown collapses to US + CA (not one row per event).
    let co = admin_get(&app, "/api/site/breakdown?site=site-abc&by=country&hours=8760").await;
    assert_eq!(co["rows"].as_array().unwrap().len(), 2);

    // Series: pageviews metric returns a zero-filled ascending array.
    let ser = admin_get(&app, "/api/site/series?site=site-abc&metric=pageviews&days=7").await;
    assert_eq!(ser["metric"], serde_json::json!("pageviews"));
    assert!(ser["series"].as_array().unwrap().len() >= 1);

    // Summary KPIs over a wide window that includes the import timestamps.
    // 2 visitors (v1,v2), 2 visits (s1,s2), 3 pageviews, 2 custom events.
    // s1 has 2 pageviews (not bounce); s2 has 1 pageview (bounce) → rate 0.5.
    // Duration = last−first event per visit: s1 4000ms, s2 1000ms → avg 2500ms.
    let sum = admin_get(&app, "/api/site/summary?site=site-abc&from=2026-01-01&to=2030-01-01").await;
    assert_eq!(sum["visitors"], serde_json::json!(2));
    assert_eq!(sum["visits"], serde_json::json!(2));
    assert_eq!(sum["views"], serde_json::json!(3));
    assert_eq!(sum["events"], serde_json::json!(2));
    assert_eq!(sum["bounce_rate"], serde_json::json!(0.5));
    assert_eq!(sum["visit_duration_ms"], serde_json::json!(2500));
    // Prev window (equal length before range) is present for dashboard deltas.
    assert!(sum["prev"].is_object());
    assert_eq!(sum["prev"]["visitors"], serde_json::json!(0));

    // Bad metric → 400; missing site → 400.
    let bad = app
        .clone()
        .oneshot(req_get("/api/site/series?site=site-abc&metric=nope", &[(k, v.as_str())]))
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    let nosite = app
        .oneshot(req_get("/api/site/events", &[(k, v.as_str())]))
        .await
        .unwrap();
    assert_eq!(nosite.status(), StatusCode::BAD_REQUEST);
    cleanup(&p);
}

#[tokio::test]
async fn site_import_dedupes_on_source_id() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let (k, v) = admin_hdr();
    let hdr = [(k, v.as_str())];

    let bare = r#"{
      "site": { "id": "site-dedupe", "name": "Dedupe", "domain": "dedupe.test" },
      "events": [
        { "type": 1, "url": "/a", "session_id": "s1", "created_at": 1783000001000 }
      ]
    }"#;
    let first = app
        .clone()
        .oneshot(req_post("/api/import/site-events", bare, &hdr))
        .await
        .unwrap();
    assert_eq!(body_json(first).await["imported"], serde_json::json!(1));

    // Same natural key plus an Umami id: stamp the id, do not insert a second row.
    let stamped = r#"{
      "site": { "id": "site-dedupe", "name": "Dedupe", "domain": "dedupe.test" },
      "events": [
        { "id": "evt-1", "type": 1, "url": "/a", "session_id": "s1", "created_at": 1783000001000 }
      ]
    }"#;
    let second = app
        .clone()
        .oneshot(req_post("/api/import/site-events", stamped, &hdr))
        .await
        .unwrap();
    assert_eq!(body_json(second).await["imported"], serde_json::json!(0));

    // Same id, different url and time: still one row.
    let retry = r#"{
      "site": { "id": "site-dedupe", "name": "Dedupe", "domain": "dedupe.test" },
      "events": [
        { "id": "evt-1", "type": 1, "url": "/other", "session_id": "s9", "created_at": 1783000009999 }
      ]
    }"#;
    let third = app
        .clone()
        .oneshot(req_post("/api/import/site-events", retry, &hdr))
        .await
        .unwrap();
    assert_eq!(body_json(third).await["imported"], serde_json::json!(0));

    let sites = admin_get(&app, "/api/sites").await;
    assert_eq!(sites["sites"][0]["events"], serde_json::json!(1));
    cleanup(&p);
}

// /api/site/events honors `days` like the other site reports (it used to read
// only `hours`, so days=7 and days=30 returned the same 30-day rows).
#[tokio::test]
async fn site_events_honors_days_window() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let (k, v) = admin_hdr();

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let day = 86_400_000_i64;
    let (recent, older) = (now - 2 * day, now - 20 * day);
    let body = format!(
        r#"{{ "site": {{ "id": "site-win", "name": "Win", "domain": "win.test" }},
             "events": [
               {{ "type": 2, "name": "recent-click", "session_id": "r1", "distinct_id": "rv1", "created_at": {recent} }},
               {{ "type": 2, "name": "older-click",  "session_id": "o1", "distinct_id": "ov1", "created_at": {older} }}
             ] }}"#
    );
    let imp = app
        .clone()
        .oneshot(req_post("/api/import/site-events", &body, &[(k, v.as_str())]))
        .await
        .unwrap();
    assert_eq!(imp.status(), StatusCode::OK);

    let names = |j: &serde_json::Value| -> Vec<String> {
        j["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["name"].as_str().unwrap().to_string())
            .collect()
    };

    let week = admin_get(&app, "/api/site/events?site=site-win&days=7").await;
    assert_eq!(names(&week), vec!["recent-click"]);
    assert_eq!(week["total"], serde_json::json!(1));

    let month = admin_get(&app, "/api/site/events?site=site-win&days=30").await;
    let mut m = names(&month);
    m.sort();
    assert_eq!(m, vec!["older-click", "recent-click"]);
    assert_eq!(month["total"], serde_json::json!(2));

    // Rolling `hours` (the dashboard's param) still works.
    let h = admin_get(&app, "/api/site/events?site=site-win&hours=72").await;
    assert_eq!(names(&h), vec!["recent-click"]);
    cleanup(&p);
}

// /api/report/site-daily rolls up all sites over the last 24h in one call.
#[tokio::test]
async fn report_site_daily_rollup() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let (k, v) = admin_hdr();

    // Events must fall inside the 24h window, so timestamp them off "now".
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let alpha = format!(
        r#"{{ "site": {{ "id": "s-alpha", "name": "Alpha", "domain": "alpha.test" }},
             "events": [
               {{ "type": 1, "url": "/",  "session_id": "a1", "distinct_id": "av1", "created_at": {t1} }},
               {{ "type": 1, "url": "/x", "session_id": "a2", "distinct_id": "av2", "created_at": {t2} }}
             ] }}"#,
        t1 = now - 1000,
        t2 = now - 2000
    );
    let beta = format!(
        r#"{{ "site": {{ "id": "s-beta", "name": "Beta", "domain": "beta.test" }},
             "events": [
               {{ "type": 1, "url": "/", "session_id": "b1", "distinct_id": "bv1", "created_at": {t} }}
             ] }}"#,
        t = now - 1500
    );
    for body in [&alpha, &beta] {
        let imp = app
            .clone()
            .oneshot(req_post("/api/import/site-events", body, &[(k, v.as_str())]))
            .await
            .unwrap();
        assert_eq!(imp.status(), StatusCode::OK);
    }

    // JSON: totals + one row per site, busiest first (Alpha 2 views > Beta 1).
    let r = admin_get(&app, "/api/report/site-daily").await;
    assert_eq!(r["totals"]["sites"], serde_json::json!(2));
    assert_eq!(r["totals"]["views"], serde_json::json!(3));
    let sites = r["sites"].as_array().unwrap();
    assert_eq!(sites.len(), 2);
    assert_eq!(sites[0]["name"], serde_json::json!("Alpha"));
    assert_eq!(sites[0]["views"], serde_json::json!(2));

    // ?format=md → text/markdown table.
    let md = app
        .clone()
        .oneshot(req_get("/api/report/site-daily?format=md", &[(k, v.as_str())]))
        .await
        .unwrap();
    assert_eq!(md.status(), StatusCode::OK);
    let ct = md
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(ct.starts_with("text/markdown"), "ct={ct}");
    let bytes = axum::body::to_bytes(md.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("# Apex Sites Daily"), "body: {text}");
    assert!(text.contains("| Site |"), "body: {text}");

    // Admin-gated: no key → 404.
    let unauth = app
        .oneshot(req_get("/api/report/site-daily", &[]))
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::NOT_FOUND);
    cleanup(&p);
}

// Phase B: POST /api/collect live ingest — write_key auth, server enrichment,
// bot drop, probe-resistant 404. GET /script.js serves the tracker.
#[tokio::test]
async fn collect_live_ingest_and_script() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let (k, v) = admin_hdr();

    // Seed a site via import so it gets a write_key; read the key back (admin).
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    let seed = format!(
        r#"{{ "site": {{ "id": "site-live", "name": "Live", "domain": "live.test" }},
             "events": [ {{ "type": 1, "url": "/seed", "session_id": "z", "distinct_id": "z", "created_at": {t} }} ] }}"#,
        t = now - 1000
    );
    let imp = app
        .clone()
        .oneshot(req_post("/api/import/site-events", &seed, &[(k, v.as_str())]))
        .await
        .unwrap();
    assert_eq!(imp.status(), StatusCode::OK);
    let sites = admin_get(&app, "/api/sites").await;
    let wk = sites["sites"][0]["write_key"].as_str().unwrap().to_string();
    assert_eq!(wk.len(), 32, "write_key is hex(randomblob(16))");

    // Live pageview with UA + CF country → one enriched row.
    let body = format!(
        r#"{{ "type": "pageview", "payload": {{ "website": "{wk}", "url": "/pricing?utm_source=x", "referrer": "google.com" }} }}"#
    );
    let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15) AppleWebKit/537 (KHTML) Chrome/126 Safari/537";
    let hit = app
        .clone()
        .oneshot(req_post("/api/collect", &body, &[("user-agent", ua), ("cf-ipcountry", "US")]))
        .await
        .unwrap();
    assert_eq!(hit.status(), StatusCode::NO_CONTENT);

    // Enrichment landed: country US, browser Chrome (server-derived, not client).
    let co = admin_get(&app, "/api/site/breakdown?site=site-live&by=country&hours=8760").await;
    assert!(co["rows"].as_array().unwrap().iter().any(|r| r["name"] == "US"));
    let br = admin_get(&app, "/api/site/breakdown?site=site-live&by=browser&hours=8760").await;
    assert!(br["rows"].as_array().unwrap().iter().any(|r| r["name"] == "Chrome"));

    // Raw UA is stored so the bot filter can be audited against what it lets
    // through; imported rows carry no UA and must not appear.
    let uas = admin_get(&app, "/api/site/user-agents?site=site-live&hours=8760").await;
    let rows = uas["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "only the /collect hit has a UA, not the import");
    assert_eq!(rows[0]["ua"], serde_json::json!(ua));
    assert_eq!(rows[0]["hits"], serde_json::json!(1));
    assert_eq!(rows[0]["hits_per_visitor"], serde_json::json!(1.0));

    // Umami website UUID is the site id. An Umami-compatible proxy forwards that id.
    let by_id = r#"{"type":"pageview","payload":{"website":"site-live","url":"/from-umami-id"}}"#;
    let id_hit = app
        .clone()
        .oneshot(req_post("/api/send", by_id, &[("user-agent", ua), ("cf-ipcountry", "US")]))
        .await
        .unwrap();
    assert_eq!(id_hit.status(), StatusCode::NO_CONTENT);

    // Unknown write_key → 404 (probe-resistant).
    let bad = app
        .clone()
        .oneshot(req_post(
            "/api/collect",
            r#"{"type":"pageview","payload":{"website":"deadbeefdeadbeef"}}"#,
            &[("user-agent", "x")],
        ))
        .await
        .unwrap();
    assert_eq!(bad.status(), StatusCode::NOT_FOUND);

    // Bot UA → accepted (204) and stored with bot=1; KPIs still exclude bots.
    let botbody =
        format!(r#"{{ "type": "pageview", "payload": {{ "website": "{wk}", "url": "/bot" }} }}"#);
    let bot = app
        .clone()
        .oneshot(req_post("/api/collect", &botbody, &[("user-agent", "Googlebot/2.1")]))
        .await
        .unwrap();
    assert_eq!(bot.status(), StatusCode::NO_CONTENT);
    let sum = admin_get(&app, "/api/site/summary?site=site-live&days=2").await;
    assert_eq!(sum["views"], serde_json::json!(3), "bot pageview excluded from KPI");
    // Audit endpoint still sees bot UA (stored for filter review).
    let uas2 = admin_get(&app, "/api/site/user-agents?site=site-live&hours=8760").await;
    assert!(
        uas2["rows"].as_array().unwrap().iter().any(|r| r["ua"].as_str().unwrap_or("").contains("Googlebot")),
        "bot UA retained for audit"
    );

    // Tracker script is public + looks right.
    let js = app.oneshot(req_get("/script.js", &[])).await.unwrap();
    assert_eq!(js.status(), StatusCode::OK);
    let ct = js.headers().get("content-type").unwrap().to_str().unwrap().to_string();
    assert!(ct.starts_with("text/javascript"), "ct={ct}");
    let bytes = axum::body::to_bytes(js.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("data-website-id"), "tracker reads website id");
    assert!(text.contains("/api/send"), "tracker posts to send (not collect — adblock)");
    assert!(text.contains("data-umami-event"), "click attr parity");
    assert!(text.contains("identify"), "apex.identify");
    assert!(
        text.contains("window.apex=window.dottie={track:"),
        "apex.track surface, with the pre-rename window.dottie alias"
    );
    assert!(text.contains("data-apex-event"), "apex click attr");
    assert!(text.contains("data-dottie-event"), "pre-rename click attr still matched");

    cleanup(&p);
}


// Crash/hang events carry a stack + log tail and exceed the 4 KiB app-wide
// cap; /api/events must accept them (up to 32 KiB) while the cap still holds
// beyond that.
#[tokio::test]
async fn events_accepts_crash_sized_bodies_but_not_unbounded() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let fat = |bytes: usize| {
        let filler = "x".repeat(bytes);
        format!(
            r#"{{"install_id":"i1","name":"error.crash","props":{{"stack":"{filler}"}}}}"#
        )
    };
    // 6 KiB: over the old 4 KiB cap, must land.
    let resp = app
        .clone()
        .oneshot(req_post("/api/events", &fat(6 * 1024), &[DESKTOP_HDR]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "6 KiB crash event must be accepted");
    // 40 KiB: over the per-route cap, still rejected.
    let resp = app
        .clone()
        .oneshot(req_post("/api/events", &fat(40 * 1024), &[DESKTOP_HDR]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    // Other routes keep the 4 KiB cap.
    let resp = app
        .clone()
        .oneshot(req_post("/api/ping", &fat(6 * 1024), &[DESKTOP_HDR]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    cleanup(&p);
}
