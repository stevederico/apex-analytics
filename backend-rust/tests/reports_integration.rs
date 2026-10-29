//! Integration tests for the A5 REPORT/READ layer — a faithful port of
//! backend/__tests__/reports.test.js, errors_by_version.test.js, and the
//! stats/retention/active cases in events.test.js.
//!
//! Two styles:
//!   - DB-LEVEL: call the real `Db::get_*` report fns directly against a temp-file
//!     `Db` (built via `Db::connect`, which runs migrate + ensure_schema, so the
//!     event_id partial unique index exists and the time-windowed engaged
//!     definition is exercised end to end).
//!   - ROUTED: drive `/api/stats/*` + `/api/diagnostics/*` through the full
//!     `build_app` router via `tower::ServiceExt::oneshot` with the admin bearer
//!     key, asserting status + shape parity with the JS suite.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use apex_analytics::app::{build_app, AppState, VersionConfig};
use apex_analytics::db::{Db, Diagnostic, Event, Ping};
use apex_analytics::reports::ReportExclude;
use serde_json::{json, Value};
use tower::ServiceExt; // for `oneshot`

const ADMIN_KEY: &str = "test-admin-key-0000000000";

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_db_path() -> String {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!("dottie_axum_a5_{}_{}.db", std::process::id(), n));
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
}

fn ev(install_id: &str, name: &str) -> Event {
    Event {
        install_id: install_id.into(),
        name: name.into(),
        version: Some("1".into()),
        ..Default::default()
    }
}

fn ev_props(install_id: &str, name: &str, version: &str, props: Value) -> Event {
    Event {
        install_id: install_id.into(),
        name: name.into(),
        version: Some(version.into()),
        props: Some(props),
        ..Default::default()
    }
}

fn req_get(uri: &str, headers: &[(&str, &str)]) -> Request<Body> {
    let mut b = Request::builder().method("GET").uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    b.body(Body::empty()).unwrap()
}

fn admin_hdr() -> Vec<(&'static str, String)> {
    vec![("authorization", format!("Bearer {ADMIN_KEY}"))]
}

async fn body_json(resp: Response<Body>) -> Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).expect("response body is JSON")
}

// Drive a GET through the full router with the admin bearer key.
async fn admin_get(db: &Db, uri: &str) -> (StatusCode, Value) {
    let app = build_app(state_for(db.clone()));
    let hdrs = admin_hdr();
    let hdr_refs: Vec<(&str, &str)> = hdrs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let resp = app.oneshot(req_get(uri, &hdr_refs)).await.unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

// ============================================================================
// reports.test.js
// ============================================================================

// event_id dedupe: same event_id inserts once; null event_id still inserts.
#[tokio::test]
async fn event_id_dedupe_and_null_path() {
    let (db, p) = fresh().await;
    let eid = "evt-abc-123";
    let mk = || Event {
        install_id: "iid-dedupe".into(),
        name: "chat-sent".into(),
        version: Some("1".into()),
        event_id: Some(eid.into()),
        ..Default::default()
    };
    db.record_event(&mk()).await.unwrap();
    db.record_event(&mk()).await.unwrap();
    db.record_event(&ev("iid-dedupe", "chat-sent")).await.unwrap();
    db.record_event(&ev("iid-dedupe", "chat-sent")).await.unwrap();
    db.record_event(&mk()).await.unwrap(); // third dup still no-ops

    let n: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE install_id = 'iid-dedupe'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(n, 3, "1 deduped event_id + 2 null-id rows");
    cleanup(&p);
}

// getFunnelReport counts distinct installs per step, ordered desc, excludes non-funnel.
#[tokio::test]
async fn funnel_report_distinct_installs_ordered() {
    let (db, p) = fresh().await;
    for iid in ["a", "a", "b", "c"] {
        db.record_event(&ev(iid, "funnel.open")).await.unwrap();
    }
    for iid in ["a", "b"] {
        db.record_event(&ev(iid, "funnel.signup")).await.unwrap();
    }
    db.record_event(&ev("a", "chat-sent")).await.unwrap();

    let report = db.get_funnel_report(24.0).await.unwrap();
    let steps = report["steps"].as_array().unwrap();
    let open = steps.iter().find(|s| s["name"] == json!("funnel.open")).unwrap();
    let signup = steps.iter().find(|s| s["name"] == json!("funnel.signup")).unwrap();
    assert_eq!(open["installs"], json!(3));
    assert_eq!(signup["installs"], json!(2));
    assert!(!steps.iter().any(|s| s["name"] == json!("chat-sent")));
    // Ordered installs desc → open before signup.
    let pos = |name: &str| steps.iter().position(|s| s["name"] == json!(name)).unwrap();
    assert!(pos("funnel.open") < pos("funnel.signup"));
    cleanup(&p);
}

// getCrashReport groups by signal + top stack frame; includes error.hang*; excludes error.thrown.
#[tokio::test]
async fn crash_report_groups_by_signal_and_top_frame() {
    let (db, p) = fresh().await;
    let stack_a = "Frame0 at foo.js:10\nFrame1 at bar.js:20";
    db.record_event(&ev_props("a", "error.crash", "2026.6.24", json!({"signal":"SIGSEGV","stack":stack_a}))).await.unwrap();
    db.record_event(&ev_props("b", "error.crash", "2026.6.24", json!({"signal":"SIGSEGV","stack":stack_a}))).await.unwrap();
    db.record_event(&ev_props("a", "error.crash", "2026.6.24", json!({"signal":"SIGABRT","stack":stack_a}))).await.unwrap();
    db.record_event(&ev_props("a", "error.hang.watchdog", "2026.6.24", json!({"signal":"WATCHDOG","stack":"MainThread at ui.js:5\nmore"}))).await.unwrap();
    db.record_event(&ev_props("a", "error.thrown", "2026.6.24", json!({"file":"x.js","line":1}))).await.unwrap();

    let report = db.get_crash_report(168.0).await.unwrap();
    let rows = report["rows"].as_array().unwrap();

    let seg = rows.iter().find(|r| r["signal"] == json!("SIGSEGV")).unwrap();
    assert_eq!(seg["count"], json!(2));
    assert_eq!(seg["installs"], json!(2));
    assert_eq!(seg["top_frame"], json!("Frame0 at foo.js:10"));

    let abrt = rows.iter().find(|r| r["signal"] == json!("SIGABRT")).unwrap();
    assert_eq!(abrt["count"], json!(1));

    let hang = rows.iter().find(|r| r["signal"] == json!("WATCHDOG")).unwrap();
    assert_eq!(hang["top_frame"], json!("MainThread at ui.js:5"));

    let total: i64 = rows.iter().map(|r| r["count"].as_i64().unwrap()).sum();
    assert_eq!(total, 4, "only crash+hang (2 SEGV + 1 ABRT + 1 hang)");
    cleanup(&p);
}

// getErrorSignatures splits the same event name by code/reason/provider.
#[tokio::test]
async fn error_signatures_split_by_code_reason() {
    let (db, p) = fresh().await;
    // Same name, two structured failure modes + one thrown with site.
    db.record_event(&ev_props(
        "a",
        "error.realtime_server_error",
        "2026.8.5.2",
        json!({"code":"llm_error","reason":"all_providers_failed","provider":"xai"}),
    ))
    .await
    .unwrap();
    db.record_event(&ev_props(
        "a",
        "error.realtime_server_error",
        "2026.8.5.2",
        json!({"code":"llm_error","reason":"all_providers_failed","provider":"xai"}),
    ))
    .await
    .unwrap();
    db.record_event(&ev_props(
        "b",
        "error.realtime_server_error",
        "2026.8.5.2",
        json!({"code":"xai_voice_unavailable","reason":"mint_failed","provider":"dottiepro"}),
    ))
    .await
    .unwrap();
    db.record_event(&ev_props(
        "c",
        "error.thrown",
        "2026.8.1.7",
        json!({"file":"App.swift","function":"boot","line":12}),
    ))
    .await
    .unwrap();

    let report = db.get_error_signatures(168.0).await.unwrap();
    assert_eq!(report["total"], json!(4));
    let rows = report["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 3, "three distinct signatures");

    let llm = rows
        .iter()
        .find(|r| r["code"] == json!("llm_error"))
        .expect("llm signature");
    assert_eq!(llm["count"], json!(2));
    assert_eq!(llm["installs"], json!(1));
    assert_eq!(llm["provider"], json!("xai"));
    assert!(llm["detail"].as_str().unwrap().contains("all_providers_failed"));

    let mint = rows
        .iter()
        .find(|r| r["code"] == json!("xai_voice_unavailable"))
        .expect("mint signature");
    assert_eq!(mint["count"], json!(1));
    assert_eq!(mint["provider"], json!("dottiepro"));

    let thrown = rows
        .iter()
        .find(|r| r["name"] == json!("error.thrown"))
        .expect("thrown signature");
    assert!(thrown["detail"].as_str().unwrap().contains("App.swift"));

    // Route surface.
    let (status, body) = admin_get(&db, "/api/stats/error-signatures?hours=24").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], json!(4));
    assert_eq!(body["window_hours"], json!(24));
    cleanup(&p);
}

// C1) An event with no code/reason/file falls back to its distinguishing props
// (bucketed) instead of the useless "(no props)" label.
#[tokio::test]
async fn error_signatures_fall_back_to_props_detail() {
    let (db, p) = fresh().await;
    for hang in [6.2, 7.9] {
        db.record_event(&ev_props(
            "a",
            "error.app_hang",
            "2026.8.8",
            json!({ "hang_s": hang }),
        ))
        .await
        .unwrap();
    }
    db.record_event(&ev_props(
        "b",
        "error.app_hang",
        "2026.8.8",
        json!({ "hang_s": 45 }),
    ))
    .await
    .unwrap();
    db.record_event(&ev_props("c", "error.app_hang", "2026.8.8", json!({})))
        .await
        .unwrap();

    let rows = db.get_error_signatures(168.0).await.unwrap()["rows"]
        .as_array()
        .unwrap()
        .clone();
    let detail = |d: &str| {
        rows.iter()
            .find(|r| r["detail"] == json!(d))
            .unwrap_or_else(|| panic!("missing detail {d}"))
            .clone()
    };
    // Same bucket collapses; a different bucket is its own signature.
    assert_eq!(detail("hang_s=5-10")["count"], json!(2));
    assert_eq!(detail("hang_s=30-60")["count"], json!(1));
    // Genuinely prop-less events still read "(no props)".
    assert_eq!(detail("(no props)")["count"], json!(1));
    cleanup(&p);
}

// Structured RSE props become distinct issue fingerprints (not one empty mega-row).
#[tokio::test]
async fn issues_fingerprint_uses_code_reason_when_no_site() {
    let (db, p) = fresh().await;
    db.record_event(&ev_props(
        "a",
        "error.realtime_server_error",
        "2026.8.5.2",
        json!({"code":"llm_error","reason":"all_providers_failed","provider":"xai"}),
    ))
    .await
    .unwrap();
    db.record_event(&ev_props(
        "b",
        "error.realtime_server_error",
        "2026.8.5.2",
        json!({"code":"xai_voice_unavailable","reason":"mint_failed","provider":"dottiepro"}),
    ))
    .await
    .unwrap();
    // Same structured signature again → bumps count on first issue only.
    db.record_event(&ev_props(
        "c",
        "error.realtime_server_error",
        "2026.8.5.2",
        json!({"code":"llm_error","reason":"all_providers_failed","provider":"xai"}),
    ))
    .await
    .unwrap();

    let issues = db.get_issues(None).await.unwrap();
    let rows = issues["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "two structured fingerprints");
    let llm = rows
        .iter()
        .find(|r| r["file"] == json!("llm_error"))
        .expect("llm issue stores code in file column");
    assert_eq!(llm["function"], json!("all_providers_failed"));
    assert_eq!(llm["line"], json!("xai"));
    assert_eq!(llm["count"], json!(2));
    assert!(llm["fingerprint"]
        .as_str()
        .unwrap()
        .contains("llm_error|all_providers_failed|xai"));
    cleanup(&p);
}

// gateway_buffered sends category/reason/component (no code/provider).
// Connection noise must not collapse TTS vs LLM into one mega-row.
#[tokio::test]
async fn issues_fingerprint_gateway_buffered_splits_connection_by_component() {
    let (db, p) = fresh().await;
    let conn = |component: &str| {
        json!({
            "category": "NETWORK",
            "reason": "connection",
            "component": component,
        })
    };
    db.record_event(&ev_props("a", "error.gateway_buffered", "2026.8.28", conn("tts")))
        .await
        .unwrap();
    db.record_event(&ev_props("a", "error.gateway_buffered", "2026.8.28", conn("tts")))
        .await
        .unwrap();
    db.record_event(&ev_props("b", "error.gateway_buffered", "2026.8.28", conn("llm")))
        .await
        .unwrap();

    let rows = db.get_issues(None).await.unwrap()["rows"]
        .as_array()
        .unwrap()
        .clone();
    assert_eq!(rows.len(), 2, "tts vs llm connection are distinct issues");
    let tts = rows
        .iter()
        .find(|r| r["line"] == json!("tts"))
        .expect("tts component stored in line");
    assert_eq!(tts["file"], json!("NETWORK"));
    assert_eq!(tts["function"], json!("connection"));
    assert_eq!(tts["count"], json!(2));
    assert!(tts["fingerprint"]
        .as_str()
        .unwrap()
        .contains("NETWORK|connection|tts"));
    let llm = rows
        .iter()
        .find(|r| r["line"] == json!("llm"))
        .expect("llm component");
    assert_eq!(llm["count"], json!(1));
    cleanup(&p);
}

// One-install gateway_buffered connection storm ranks below a multi-install
// real error (blast radius first, then count).
#[tokio::test]
async fn error_signatures_rank_installs_ahead_of_connection_volume() {
    let (db, p) = fresh().await;
    for _ in 0..5 {
        db.record_event(&ev_props(
            "storm",
            "error.gateway_buffered",
            "2026.8.28",
            json!({"category":"NETWORK","reason":"connection","component":"tts"}),
        ))
        .await
        .unwrap();
    }
    db.record_event(&ev_props(
        "u1",
        "error.thrown",
        "2026.8.28",
        json!({"file":"Real.swift","function":"boot","line":12}),
    ))
    .await
    .unwrap();
    db.record_event(&ev_props(
        "u2",
        "error.thrown",
        "2026.8.28",
        json!({"file":"Real.swift","function":"boot","line":12}),
    ))
    .await
    .unwrap();

    let rows = db.get_error_signatures(168.0).await.unwrap()["rows"]
        .as_array()
        .unwrap()
        .clone();
    assert!(rows.len() >= 2, "storm + real error");
    assert_eq!(rows[0]["name"], json!("error.thrown"), "blast radius first: {rows:?}");
    assert_eq!(rows[0]["installs"], json!(2));
    let conn = rows
        .iter()
        .find(|r| r["name"] == json!("error.gateway_buffered"))
        .expect("connection signature");
    assert_eq!(conn["code"], json!("NETWORK"));
    assert_eq!(conn["reason"], json!("connection"));
    assert_eq!(conn["provider"], json!("tts"));
    assert_eq!(conn["count"], json!(5));
    assert_eq!(conn["installs"], json!(1));
    cleanup(&p);
}

// getNewErrorSignatures excludes signatures present in the prior week.
#[tokio::test]
async fn new_error_signatures_excludes_prior_week() {
    let (db, p) = fresh().await;
    let day = 24 * 60 * 60 * 1000i64;
    let now: i64 = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();

    // KNOWN signature: seen in the prior week AND today → must be EXCLUDED.
    db.record_event(&ev_props("old", "error.thrown", "1", json!({"file":"Known.swift","line":42}))).await.unwrap();
    // Back-date the most-recent Known row into the prior-week window (3d ago).
    sqlx::query(
        "UPDATE events SET created_at = ?
         WHERE id = (SELECT id FROM events
                     WHERE name='error.thrown'
                       AND json_extract(props,'$.file')='Known.swift'
                       AND json_extract(props,'$.line')=42
                     ORDER BY id DESC LIMIT 1)",
    )
    .bind(now - 3 * day)
    .execute(db.pool())
    .await
    .unwrap();
    db.record_event(&ev_props("new", "error.thrown", "1", json!({"file":"Known.swift","line":42}))).await.unwrap(); // today

    // NEW signature: only today → must be INCLUDED.
    db.record_event(&ev_props("new", "error.thrown", "1", json!({"file":"Fresh.swift","line":7}))).await.unwrap();
    db.record_event(&ev_props("old", "error.thrown", "1", json!({"file":"Fresh.swift","line":7}))).await.unwrap();

    let report = db.get_new_error_signatures().await.unwrap();
    let rows = report["rows"].as_array().unwrap();

    let fresh = rows
        .iter()
        .find(|r| r["file"] == json!("Fresh.swift") && r["line"] == json!(7))
        .expect("fresh signature reported");
    assert_eq!(fresh["count"], json!(2));
    assert_eq!(fresh["installs"], json!(2));
    assert!(
        !rows.iter().any(|r| r["file"] == json!("Known.swift")),
        "prior-week signature excluded"
    );
    cleanup(&p);
}

// getActivationFunnel clamps the window.
#[tokio::test]
async fn activation_funnel_clamps_window() {
    let (db, p) = fresh().await;
    assert_eq!(db.get_activation_funnel(0.0).await.unwrap()["window_hours"], json!(1));
    assert_eq!(db.get_activation_funnel(99999.0).await.unwrap()["window_hours"], json!(720));
    // Fractional window is KEPT (Number coercion, no floor) and echoed as a float.
    assert_eq!(db.get_activation_funnel(48.5).await.unwrap()["window_hours"], json!(48.5));

    // Routed (parseIntQuery path): non-numeric "x" → Number("x") NaN → fallback 24;
    // empty "hours=" → fallback 24.
    let (_, nan) = admin_get(&db, "/api/stats/activation?hours=x").await;
    assert_eq!(nan["window_hours"], json!(24), "NaN activation hours → 24 fallback");
    let (_, empty) = admin_get(&db, "/api/stats/activation?hours=").await;
    assert_eq!(empty["window_hours"], json!(24), "empty activation hours → 24 fallback");
    // Fractional survives the route + clamp.
    let (_, frac) = admin_get(&db, "/api/stats/activation?hours=48.5").await;
    assert_eq!(frac["window_hours"], json!(48.5), "fractional activation window kept");
    cleanup(&p);
}

// getActivationFunnel: a user who signs up then fires chat-sent is activated,
// with an ISO signed_up_at and a non-negative time_to_activation_ms.
#[tokio::test]
async fn activation_funnel_activated_user_shape() {
    let (db, p) = fresh().await;
    db.register_user(&apex_analytics::db::RegisterUser {
        name: "Real".into(),
        email: "real@example.com".into(),
        install_id: "act-1".into(),
        version: Some("1".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    db.record_event(&ev("act-1", "chat-sent")).await.unwrap();
    db.record_ping(&Ping { install_id: "act-1".into(), version: Some("1".into()) }).await.unwrap();

    let report = db.get_activation_funnel(24.0).await.unwrap();
    assert_eq!(report["total"], json!(1));
    assert_eq!(report["activated"], json!(1));
    // activation_rate = 1/1 = 1 → integer token, not 1.0.
    assert_eq!(report["activation_rate"], json!(1));
    let user = &report["users"][0];
    assert_eq!(user["activated"], json!(true));
    assert!(user["signed_up_at"].as_str().unwrap().ends_with('Z'));
    assert!(user["first_activation_at"].as_str().unwrap().ends_with('Z'));
    assert!(user["time_to_activation_ms"].as_i64().unwrap() >= 0);
    assert_eq!(user["ping_count"], json!(1));
    cleanup(&p);
}

// Diagnostics list + detail round-trip incl. non-existent / non-integer id.
#[tokio::test]
async fn diagnostics_list_and_detail_roundtrip() {
    let (db, p) = fresh().await;
    db.record_diagnostic(&Diagnostic {
        install_id: "diag-1".into(),
        version: Some("2026.6.24".into()),
        fingerprint: Some(json!({"cpu":"M3","ram_gb":16})),
        logs: Some(json!({"gateway":"tail..."})),
        gateway: Some(json!({"errors":3})),
        note: Some("stuck on boot".into()),
    })
    .await
    .unwrap();

    let list = db.get_diagnostics_list(50.0).await.unwrap();
    let mine = list.iter().find(|r| r["install_id"] == json!("diag-1")).unwrap();
    assert_eq!(mine["fingerprint"], json!({"cpu":"M3","ram_gb":16}));
    let id = mine["id"].as_i64().unwrap();

    let full = db.get_diagnostic(id).await.unwrap().unwrap();
    assert_eq!(full["note"], json!("stuck on boot"));
    assert_eq!(full["gateway"], json!({"errors":3}));
    assert_eq!(full["logs"], json!({"gateway":"tail..."}));

    assert!(db.get_diagnostic(999999).await.unwrap().is_none(), "missing id → None");

    // Routed: "<id>.0" is a valid integer per Number.isInteger(Number("12.0")) and
    // must resolve to the same row as the bare integer id.
    let (status_dot0, body_dot0) = admin_get(&db, &format!("/api/diagnostics/{id}.0")).await;
    assert_eq!(status_dot0, StatusCode::OK, "trailing .0 id resolves");
    assert_eq!(body_dot0["id"], json!(id));
    // Routed: a true fractional id is NOT an integer → 404.
    let (status_frac, _) = admin_get(&db, &format!("/api/diagnostics/{id}.5")).await;
    assert_eq!(status_frac, StatusCode::NOT_FOUND, "fractional id → 404");
    cleanup(&p);
}

// Routed: /api/diagnostics/:id 404 for non-existent AND non-integer id.
#[tokio::test]
async fn diagnostics_detail_route_404s() {
    let (db, p) = fresh().await;
    let (status, body) = admin_get(&db, "/api/diagnostics/999999").await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"], json!("not_found"));

    let (status2, body2) = admin_get(&db, "/api/diagnostics/nope").await;
    assert_eq!(status2, StatusCode::NOT_FOUND, "non-integer id → 404");
    assert_eq!(body2["error"], json!("not_found"));
    cleanup(&p);
}

// Routed: /api/diagnostics/list wraps rows in { rows }.
#[tokio::test]
async fn diagnostics_list_route_wraps_rows() {
    let (db, p) = fresh().await;
    db.record_diagnostic(&Diagnostic {
        install_id: "diag-route".into(),
        version: Some("1".into()),
        fingerprint: Some(json!({"cpu":"M1"})),
        ..Default::default()
    })
    .await
    .unwrap();
    let (status, body) = admin_get(&db, "/api/diagnostics/list").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["rows"].is_array());
    assert!(body["rows"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["install_id"] == json!("diag-route")));
    cleanup(&p);
}

// ============================================================================
// errors_by_version.test.js
// ============================================================================

#[tokio::test]
async fn errors_by_version_groups_and_excludes_non_errors() {
    let (db, p) = fresh().await;
    for _ in 0..2 {
        db.record_event(&ev_props("a", "error.conversation_engine_init_failed", "2026.5.29.1", json!({}))).await.unwrap();
    }
    db.record_event(&ev_props("b", "error.conversation_engine_init_failed", "2026.5.29.1", json!({}))).await.unwrap();
    db.record_event(&ev_props("a", "error.conversation_engine_init_failed", "2026.5.13", json!({}))).await.unwrap();
    db.record_event(&ev_props("a", "chat-sent", "2026.5.29.1", json!({}))).await.unwrap();

    let report = db.get_errors_by_version(168.0).await.unwrap();
    let rows = report["rows"].as_array().unwrap();
    let cur = rows.iter().find(|r| r["version"] == json!("2026.5.29.1") && r["name"] == json!("error.conversation_engine_init_failed")).unwrap();
    let old = rows.iter().find(|r| r["version"] == json!("2026.5.13") && r["name"] == json!("error.conversation_engine_init_failed")).unwrap();
    assert_eq!(cur["count"], json!(3));
    assert_eq!(cur["installs"], json!(2));
    assert_eq!(old["count"], json!(1));
    assert_eq!(old["installs"], json!(1));
    assert!(!rows.iter().any(|r| r["name"] == json!("chat-sent")));
    cleanup(&p);
}

#[tokio::test]
async fn errors_by_version_clamps_window() {
    let (db, p) = fresh().await;
    assert_eq!(db.get_errors_by_version(0.0).await.unwrap()["window_hours"], json!(1));
    assert_eq!(db.get_errors_by_version(99999.0).await.unwrap()["window_hours"], json!(2160));
    cleanup(&p);
}

// ============================================================================
// events.test.js — stats / errors / retention / active routed cases
// ============================================================================

#[tokio::test]
async fn stats_shape_with_admin_key() {
    let (db, p) = fresh().await;
    let (status, body) = admin_get(&db, "/api/stats").await;
    assert_eq!(status, StatusCode::OK);
    for k in ["users", "dau", "mau", "wau", "pings_24h", "events_24h", "stickiness"] {
        assert!(body.get(k).is_some(), "missing {k}");
    }
    assert!(body["top_events_24h"].is_array());
    assert!(body["countries"].is_array());
    assert!(body["new_users_24h_by_country"].is_array());
    cleanup(&p);
}

#[tokio::test]
async fn stats_404_without_admin_key() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app.oneshot(req_get("/api/stats", &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    cleanup(&p);
}

// Engaged metrics + stickiness: a ping + chat-sent makes dau_engaged ≥ 1; a
// voice.activated install bumps it again. Exercises the time-windowed engaged def.
#[tokio::test]
async fn stats_engaged_and_stickiness() {
    let (db, p) = fresh().await;
    db.record_ping(&Ping { install_id: "eng-1".into(), version: Some("9.62".into()) }).await.unwrap();
    db.record_event(&ev("eng-1", "chat-sent")).await.unwrap();

    let s1 = db.get_stats().await.unwrap();
    assert!(s1["dau_engaged"].as_i64().unwrap() >= 1, "chat-sent install counts");
    for k in ["wau", "dau_engaged", "wau_engaged", "mau_engaged", "stickiness"] {
        assert!(s1.get(k).is_some(), "missing {k}");
    }
    // stickiness is a ratio in [0,1] or null.
    if !s1["stickiness"].is_null() {
        let v = s1["stickiness"].as_f64().unwrap();
        assert!((0.0..=1.0).contains(&v));
    }

    db.record_ping(&Ping { install_id: "eng-2".into(), version: Some("9.62".into()) }).await.unwrap();
    db.record_event(&ev("eng-2", "voice.activated")).await.unwrap();
    let s2 = db.get_stats().await.unwrap();
    assert!(s2["dau_engaged"].as_i64().unwrap() >= 2, "voice.activated bumps engaged");
    cleanup(&p);
}

#[tokio::test]
async fn stats_errors_clamps_and_groups() {
    let (db, p) = fresh().await;
    // Default 24 when missing.
    let (_, def) = admin_get(&db, "/api/stats/errors").await;
    assert_eq!(def["window_hours"], json!(24));
    // Clamp.
    let (_, small) = admin_get(&db, "/api/stats/errors?hours=0").await;
    assert_eq!(small["window_hours"], json!(1));
    let (_, huge) = admin_get(&db, "/api/stats/errors?hours=9999").await;
    assert_eq!(huge["window_hours"], json!(720));
    // SPECIAL errors route: Number() coercion straight into getErrorBreakdown.
    // Non-numeric "x" → NaN → 24 (the db-layer default, NOT the clamp floor).
    let (_, nan) = admin_get(&db, "/api/stats/errors?hours=x").await;
    assert_eq!(nan["window_hours"], json!(24), "NaN hours → 24 default");
    // Empty "hours=" → Number("") → 0 → clamp → 1.
    let (_, empty) = admin_get(&db, "/api/stats/errors?hours=").await;
    assert_eq!(empty["window_hours"], json!(1), "empty hours → 0 → clamp 1");
    // Fractional "48.5" is KEPT (no floor) and echoed as a float, not "48".
    let (_, frac) = admin_get(&db, "/api/stats/errors?hours=48.5").await;
    assert_eq!(frac["window_hours"], json!(48.5), "fractional window kept");

    // Grouping: three identical error.thrown collapse into one group with count≥3.
    for i in 0..3 {
        db.record_event(&ev_props(
            &format!("install-{i}"),
            "error.thrown",
            "9.27",
            json!({"file":"TestSite.swift","function":"testFn()","line":"7"}),
        ))
        .await
        .unwrap();
    }
    let (status, body) = admin_get(&db, "/api/stats/errors?hours=1").await;
    assert_eq!(status, StatusCode::OK);
    let m = body["top"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["file"] == json!("TestSite.swift") && r["function"] == json!("testFn()") && r["line"] == json!("7"))
        .expect("grouped row");
    assert!(m["count"].as_i64().unwrap() >= 3);
    cleanup(&p);
}

#[tokio::test]
async fn retention_shape_and_clamp() {
    let (db, p) = fresh().await;
    let (status, body) = admin_get(&db, "/api/stats/retention").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["cohorts"].is_array());
    assert!(body.get("generated_at").is_some());

    // Seed a ping so there's at least one cohort, then assert week format + bucket keys.
    db.record_ping(&Ping { install_id: "ret-1".into(), version: Some("1".into()) }).await.unwrap();
    let (_, body2) = admin_get(&db, "/api/stats/retention").await;
    for c in body2["cohorts"].as_array().unwrap() {
        let week = c["week"].as_str().unwrap();
        assert_eq!(week.len(), 10, "week YYYY-MM-DD");
        assert!(c["size"].is_number());
        assert!(c["engaged"].is_number());
        for k in ["d1", "d7", "d30"] {
            assert!(c[k].get("rate").is_some());
            assert!(c[k].get("returned").is_some());
            assert!(c[k].get("of").is_some());
        }
    }

    // Clamp weeks to [1,26].
    let (_, small) = admin_get(&db, "/api/stats/retention?weeks=0").await;
    assert!(small["cohorts"].as_array().unwrap().len() <= 1);
    let (_, huge) = admin_get(&db, "/api/stats/retention?weeks=999").await;
    assert!(huge["cohorts"].as_array().unwrap().len() <= 26);
    cleanup(&p);
}

#[tokio::test]
async fn active_n_rows_and_clamp() {
    let (db, p) = fresh().await;
    let (status, body) = admin_get(&db, "/api/stats/active?days=7").await;
    assert_eq!(status, StatusCode::OK);
    let days = body["days"].as_array().unwrap();
    assert_eq!(days.len(), 7);
    for d in days {
        assert_eq!(d["date"].as_str().unwrap().len(), 10);
        assert!(d["dau"].is_number());
        assert!(d["dau_engaged"].is_number());
        assert!(d["dau_engaged"].as_i64().unwrap() <= d["dau"].as_i64().unwrap());
    }
    // Clamp.
    let (_, small) = admin_get(&db, "/api/stats/active?days=0").await;
    assert_eq!(small["days"].as_array().unwrap().len(), 1);
    let (_, huge) = admin_get(&db, "/api/stats/active?days=500").await;
    assert_eq!(huge["days"].as_array().unwrap().len(), 180);
    cleanup(&p);
}

// All admin report routes 404 without the admin key.
#[tokio::test]
async fn all_report_routes_404_without_key() {
    let (db, p) = fresh().await;
    for uri in [
        "/api/stats",
        "/api/stats/errors",
        "/api/stats/retention",
        "/api/stats/active",
        "/api/stats/errors-by-version",
        "/api/stats/activation",
        "/api/stats/funnel",
        "/api/stats/crashes",
        "/api/stats/new-errors",
        "/api/diagnostics/list",
        "/api/diagnostics/1",
    ] {
        let app = build_app(state_for(db.clone()));
        let resp = app.oneshot(req_get(uri, &[])).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri} must 404 w/o admin key");
    }
    cleanup(&p);
}

// Routed smoke: the remaining stats routes all 200 with the admin key and carry
// their window_hours default.
#[tokio::test]
async fn remaining_stats_routes_200_with_defaults() {
    let (db, p) = fresh().await;
    let cases = [
        ("/api/stats/errors-by-version", 168i64),
        ("/api/stats/error-signatures", 168),
        ("/api/stats/activation", 24),
        ("/api/stats/funnel", 24),
        ("/api/stats/crashes", 168),
    ];
    for (uri, default_hours) in cases {
        let (status, body) = admin_get(&db, uri).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert_eq!(body["window_hours"], json!(default_hours), "{uri} default window");
    }
    let (status, body) = admin_get(&db, "/api/stats/new-errors").await;
    assert_eq!(status, StatusCode::OK);
    assert!(body["rows"].is_array());
    cleanup(&p);
}

// /api/stats/series: zero-filled ascending window, days clamp, from/to span,
// bad-metric 400, and dau parity with /api/stats/active.
#[tokio::test]
async fn series_zero_fill_range_and_metric_validation() {
    let (db, p) = fresh().await;

    // Empty DB: 30-day default is contiguous, ascending, all counts 0.
    let (status, body) = admin_get(&db, "/api/stats/series?metric=events").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["metric"], json!("events"));
    let s = body["series"].as_array().unwrap();
    assert_eq!(s.len(), 30);
    let mut prev = String::new();
    for row in s {
        assert_eq!(row["date"].as_str().unwrap().len(), 10);
        assert_eq!(row["count"].as_i64().unwrap(), 0);
        assert!(row["date"].as_str().unwrap().to_string() > prev, "ascending dates");
        prev = row["date"].as_str().unwrap().to_string();
    }

    // days clamp [1,180].
    let (_, small) = admin_get(&db, "/api/stats/series?metric=pings&days=0").await;
    assert_eq!(small["series"].as_array().unwrap().len(), 1);
    let (_, huge) = admin_get(&db, "/api/stats/series?metric=pings&days=500").await;
    assert_eq!(huge["series"].as_array().unwrap().len(), 180);

    // from/to span is inclusive of both end days (10 days).
    let (_, ranged) = admin_get(
        &db,
        "/api/stats/series?metric=signups&from=2026-07-01&to=2026-07-10",
    )
    .await;
    let rs = ranged["series"].as_array().unwrap();
    assert_eq!(rs.len(), 10);
    assert_eq!(rs[0]["date"], json!("2026-07-01"));
    assert_eq!(rs[9]["date"], json!("2026-07-10"));

    // Bad / missing metric → 400.
    let (bad, _) = admin_get(&db, "/api/stats/series?metric=bogus").await;
    assert_eq!(bad, StatusCode::BAD_REQUEST);
    let (missing, _) = admin_get(&db, "/api/stats/series").await;
    assert_eq!(missing, StatusCode::BAD_REQUEST);

    // Two pings today (same install) → today's bucket: dau=1, pings=2. Today is the
    // last row of the window; it must agree with /api/stats/active's dau.
    db.record_ping(&Ping { install_id: "sx-1".into(), version: Some("1".into()) })
        .await
        .unwrap();
    db.record_ping(&Ping { install_id: "sx-1".into(), version: Some("1".into()) })
        .await
        .unwrap();
    let (_, dau_s) = admin_get(&db, "/api/stats/series?metric=dau&days=7").await;
    let (_, pings_s) = admin_get(&db, "/api/stats/series?metric=pings&days=7").await;
    let dau_last = dau_s["series"].as_array().unwrap().last().unwrap();
    let pings_last = pings_s["series"].as_array().unwrap().last().unwrap();
    assert_eq!(dau_last["count"].as_i64().unwrap(), 1, "distinct install");
    assert_eq!(pings_last["count"].as_i64().unwrap(), 2, "raw ping count");
    let (_, active) = admin_get(&db, "/api/stats/active?days=7").await;
    let active_today = &active["days"].as_array().unwrap()[0]; // newest-first
    assert_eq!(active_today["dau"], dau_last["count"], "series dau == /active dau");

    cleanup(&p);
}

// /api/report/daily composites the full daily-runbook in one call.
#[tokio::test]
async fn report_daily_composite_shape() {
    let (db, p) = fresh().await;
    let (status, body) = admin_get(&db, "/api/report/daily").await;
    assert_eq!(status, StatusCode::OK);
    for k in [
        "window_hours",
        "stats",
        "errors_24h",
        "active_7d",
        "activation_24h",
        "activity",
        "errors_by_version_7d",
        "error_signatures_7d",
        "new_errors",
    ] {
        assert!(body.get(k).is_some(), "missing {k}");
    }
    assert!(body["stats"]["users"].is_number());
    assert_eq!(body["active_7d"]["days"].as_array().unwrap().len(), 7);
    assert_eq!(body["errors_24h"]["window_hours"].as_f64().unwrap(), 24.0);
    assert!(body["activity"]["new_signups_24h"].is_array());
    assert!(body["activity"]["active_24h"].is_array());
    assert!(body["errors_by_version_7d"]["rows"].is_array());
    assert!(body["error_signatures_7d"]["rows"].is_array());
    assert!(body["new_errors"]["rows"].is_array());
    cleanup(&p);
}

// Person lists surface new signups + last-seen, and exclude operator noise.
#[tokio::test]
async fn report_daily_activity_lists_and_excludes() {
    let (db, p) = fresh().await;
    // Injected, never read from process env, so parallel tests cannot race.
    let db = db.with_report_exclude(ReportExclude::new(
        " operator@example.com ,",
        "00000000-0000-0000-0000-000000000001",
    ));
    db.register_user(&apex_analytics::db::RegisterUser {
        name: "Real".into(),
        email: "real@example.com".into(),
        install_id: "act-real-1".into(),
        version: Some("2026.7.28".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    db.record_ping(&Ping {
        install_id: "act-real-1".into(),
        version: Some("2026.7.28".into()),
    })
    .await
    .unwrap();
    // Operator noise — must not appear in person lists. One matches by email
    // (different case), one by install id only.
    db.register_user(&apex_analytics::db::RegisterUser {
        name: "Operator".into(),
        email: "Operator@Example.com".into(),
        install_id: "act-operator-1".into(),
        version: Some("9.0".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    db.record_ping(&Ping {
        install_id: "act-operator-1".into(),
        version: Some("9.0".into()),
    })
    .await
    .unwrap();
    db.register_user(&apex_analytics::db::RegisterUser {
        name: "Dev box".into(),
        email: "devbox@example.com".into(),
        install_id: "00000000-0000-0000-0000-000000000001".into(),
        version: Some("9.0".into()),
        ..Default::default()
    })
    .await
    .unwrap();
    db.record_ping(&Ping {
        install_id: "00000000-0000-0000-0000-000000000001".into(),
        version: Some("9.0".into()),
    })
    .await
    .unwrap();

    let (status, body) = admin_get(&db, "/api/report/daily").await;
    assert_eq!(status, StatusCode::OK);
    let news = body["activity"]["new_signups_24h"].as_array().unwrap();
    let active = body["activity"]["active_24h"].as_array().unwrap();
    assert_eq!(news.len(), 1, "only real signup");
    assert_eq!(news[0]["email"], "real@example.com");
    assert_eq!(active.len(), 1, "only real last-seen");
    assert_eq!(active[0]["email"], "real@example.com");
    let md = {
        let app = build_app(state_for(db));
        let hdrs = admin_hdr();
        let hdr_refs: Vec<(&str, &str)> = hdrs.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let resp = app
            .oneshot(req_get("/api/report/daily?format=md", &hdr_refs))
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    };
    assert!(md.contains("real@example.com"), "md lists real user");
    assert!(!md.to_lowercase().contains("operator@example.com"), "md omits operator noise");
    assert!(!md.contains("devbox@example.com"), "md omits excluded install");
    assert!(md.contains("## New Signups (24h)"), "md: {md}");
    assert!(md.contains("## Active Last-Seen (24h)"), "md: {md}");
    assert!(md.contains("## Errors by version (7d)"), "md: {md}");
    assert!(md.contains("## New error signatures"), "md: {md}");
    cleanup(&p);
}

#[tokio::test]
async fn report_daily_404_without_key() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app.oneshot(req_get("/api/report/daily", &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    cleanup(&p);
}

// ?format=md returns a text/markdown body with the runbook headings.
#[tokio::test]
async fn report_daily_md_format() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db.clone()));
    let hdrs = admin_hdr();
    let hdr_refs: Vec<(&str, &str)> = hdrs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let resp = app
        .oneshot(req_get("/api/report/daily?format=md", &hdr_refs))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    assert!(ct.starts_with("text/markdown"), "ct={ct}");
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
    let text = String::from_utf8(bytes.to_vec()).unwrap();
    assert!(text.contains("# Dottie Daily Report"), "body: {text}");
    assert!(text.contains("## Countries"), "body: {text}");
    assert!(text.contains("## New Signups (24h)"), "body: {text}");
    assert!(text.contains("## Active Last-Seen (24h)"), "body: {text}");
    assert!(text.contains("## Errors by version (7d)"), "body: {text}");
    assert!(text.contains("## Error signatures (7d)"), "body: {text}");
    assert!(text.contains("## New error signatures"), "body: {text}");
    assert!(text.contains("DAU 0 (rolling 24h)"), "body: {text}");
    assert!(
        text.contains("## Refund requests (pending — approve/deny via /api/ops/pro/refunds/{id})"),
        "body: {text}"
    );
    assert!(text.contains("## DAU trend (7d, UTC calendar days)"), "body: {text}");
    assert!(text.contains("## Pro fleet"), "body: {text}");
    assert!(text.contains("- unavailable"), "no Pro mock → unavailable: {text}");
    cleanup(&p);
}

#[tokio::test]
async fn report_daily_fetches_pro_stats_from_the_relay() {
    // The split: analytics no longer reads pro_usage. A loopback stand-in for
    // the Pro service proves the report maps the live payload.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let payload = json!({
        "lifetime": {
            "requests": 827, "tokens": 1000,
            "cost_usd": 1.0354454, "cost_cap_usd": 25, "request_cap": 10000
        },
        "today": {
            "requests": 2, "tokens": 10,
            "cost_usd": 0.000952, "cost_cap_usd": 5, "request_cap": 400
        },
        "refunds_pending": []
    });
    let app = axum::Router::new().route(
        "/api/ops/pro/stats",
        axum::routing::get({
            let payload = payload.clone();
            move |headers: axum::http::HeaderMap| {
                let payload = payload.clone();
                async move {
                    let auth = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    assert_eq!(auth, format!("Bearer {ADMIN_KEY}"));
                    axum::Json(payload)
                }
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let (db, p) = fresh().await;
    let state = state_for(db.clone()).with_pro_stats_url(format!(
        "http://127.0.0.1:{port}/api/ops/pro/stats"
    ));
    let app = build_app(state);
    let hdrs = admin_hdr();
    let hdr_refs: Vec<(&str, &str)> = hdrs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let resp = app
        .oneshot(req_get("/api/report/daily?format=md", &hdr_refs))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let text = String::from_utf8(
        axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap()
            .to_vec(),
    )
    .unwrap();
    assert!(text.contains("lifetime 827/10000 req · $1.0354454/$25"), "{text}");
    assert!(text.contains("today 2/400 req · $0.000952/$5"), "{text}");
    assert!(text.contains("## Refund requests"), "{text}");
    assert!(text.contains("- none"), "{text}");
    cleanup(&p);
}
