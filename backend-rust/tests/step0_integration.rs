//! Step 0 integration tests.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use apex_analytics::app::{build_app, AppState, VersionConfig};
use apex_analytics::db::Db;
use tower::ServiceExt; // for `oneshot`

const ADMIN_KEY: &str = "test-admin-key-0000000000";

const LANDING_ALLOWED_ORIGIN: &str = "https://www.example.com";

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_db_path() -> String {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!("dottie_step0_{}_{}.db", std::process::id(), n));
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

// ---- request helpers --------------------------------------------------------

/// POST a raw JSON string body with the given extra headers.
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


async fn body_json(resp: Response<Body>) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    if bytes.is_empty() {
        return serde_json::Value::Null;
    }
    serde_json::from_slice(&bytes).expect("response body is JSON")
}

// Step 0: signup → install linking, experiment variant, funnel.first_success.

use apex_analytics::variant::{variant_for, CONTROL, FIRST_SUCCESS, TEST};

const DAY: i64 = 86_400_000;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

/// First `prefix-N` install id that hashes into `arm`.
fn install_in(arm: &str, prefix: &str) -> String {
    (0..1000)
        .map(|i| format!("{prefix}-{i}"))
        .find(|iid| variant_for(iid) == Some(arm))
        .unwrap()
}

fn pro_token(secret: &str, user_id: i64, email: &str) -> String {
    apex_analytics::jwt::mint(secret, user_id, email, None, now_ms() / 1000).unwrap()
}

async fn install_of(db: &Db, user_id: i64) -> String {
    sqlx::query_scalar::<_, String>("SELECT install_id FROM users WHERE id = ?")
        .bind(user_id)
        .fetch_one(db.pool())
        .await
        .unwrap()
}

async fn insert_event(db: &Db, iid: &str, name: &str, at: i64) {
    sqlx::query("INSERT INTO events (install_id, name, created_at) VALUES (?, ?, ?)")
        .bind(iid)
        .bind(name)
        .bind(at)
        .execute(db.pool())
        .await
        .unwrap();
}

#[tokio::test]
async fn pro_signup_links_to_the_install_that_pings_with_its_bearer() {
    // Signup on dottie-pro: analytics only ever sees the JWT, which has no
    // install id, so the stubbed row starts unlinked.
    let (mut db, p) = fresh().await;
    db.jwt_secret = "s0".into();
    let token = pro_token("s0", 41, "pro@test.local");
    let bearer = format!("Bearer {token}");
    let app = build_app(state_for(db.clone()));

    let resp = app
        .clone()
        .oneshot(req_post(
            "/api/ping",
            r#"{"install_id":"mac-a","version":"2026.9.14"}"#,
            &[("authorization", &bearer)],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(install_of(&db, 41).await, "mac-a");

    // Fill-only: a second Mac on the same account does not move the link.
    let resp = app
        .oneshot(req_post(
            "/api/ping",
            r#"{"install_id":"mac-b","version":"2026.9.14"}"#,
            &[("authorization", &bearer)],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(install_of(&db, 41).await, "mac-a");
    cleanup(&p);
}

#[tokio::test]
async fn events_and_batch_link_too_but_the_retired_shared_key_is_404() {
    let (mut db, p) = fresh().await;
    db.jwt_secret = "s0".into();
    let app = build_app(state_for(db.clone()));

    let t1 = pro_token("s0", 51, "one@test.local");
    let resp = app
        .clone()
        .oneshot(req_post(
            "/api/events",
            r#"{"install_id":"ev-1","name":"funnel.app_launched"}"#,
            &[("authorization", &format!("Bearer {t1}"))],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(install_of(&db, 51).await, "ev-1");

    let t2 = pro_token("s0", 52, "two@test.local");
    let resp = app
        .clone()
        .oneshot(req_post(
            "/api/events/batch",
            r#"{"events":[{"install_id":"batch-1","name":"funnel.app_launched"}]}"#,
            &[("authorization", &format!("Bearer {t2}"))],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(install_of(&db, 52).await, "batch-1");

    // The retired shared desktop key is rejected outright: nothing to link.
    let resp = app
        .oneshot(req_post(
            "/api/ping",
            r#"{"install_id":"anon","version":"1"}"#,
            &[("x-dottie-client", "test-desktop-key-00000000")],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE install_id = 'anon'")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(n, 0);
    cleanup(&p);
}

#[tokio::test]
async fn every_event_carries_the_server_variant() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db.clone()));
    let iid = install_in(TEST, "v");
    for body in [
        format!(r#"{{"install_id":"{iid}","name":"a.no_props"}}"#),
        format!(
            r#"{{"install_id":"{iid}","name":"b.client_lies","props":{{"variant":"bogus","k":"v"}}}}"#
        ),
    ] {
        let resp = app
            .clone()
            .oneshot(req_post("/api/events", &body, &[DESKTOP_HDR]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT name, props FROM events WHERE install_id = ? ORDER BY name")
            .bind(&iid)
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(rows.len(), 2);
    for (_, props) in &rows {
        let v: serde_json::Value = serde_json::from_str(props).unwrap();
        assert_eq!(v["variant"], serde_json::json!(TEST));
    }
    let kept: serde_json::Value = serde_json::from_str(&rows[1].1).unwrap();
    assert_eq!(kept["k"], serde_json::json!("v"));
    cleanup(&p);
}

#[tokio::test]
async fn first_success_is_kept_once_per_install() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db.clone()));
    let body = format!(r#"{{"install_id":"fs-1","name":"{FIRST_SUCCESS}"}}"#);
    for _ in 0..3 {
        let resp = app
            .clone()
            .oneshot(req_post("/api/events", &body, &[DESKTOP_HDR]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
    let n: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM events WHERE install_id = 'fs-1' AND name = ?")
            .bind(FIRST_SUCCESS)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(n, 1);
    cleanup(&p);
}

#[tokio::test]
async fn funnel_metrics_split_by_variant() {
    let (db, p) = fresh().await;
    let t0 = now_ms() - 3 * DAY;
    let c1 = install_in(CONTROL, "c");
    let c2 = install_in(CONTROL, "cc");
    let x1 = install_in(TEST, "x");

    // control: 2 launched, 2 attempted, 1 blocked, 1 onboarded, 1 success in 24h.
    for iid in [&c1, &c2] {
        insert_event(&db, iid, "funnel.app_launched", t0).await;
        insert_event(&db, iid, "funnel.first_chat_attempt", t0 + 1000).await;
    }
    insert_event(&db, &c1, "funnel.chat_blocked_on_gateway", t0 + 2000).await;
    insert_event(&db, &c1, "funnel.onboarding_completed", t0 + 500).await;
    insert_event(&db, &c2, FIRST_SUCCESS, t0 + 3000).await;
    // test: 1 launched, success only after 24h (does not count).
    insert_event(&db, &x1, "funnel.app_launched", t0).await;
    insert_event(&db, &x1, FIRST_SUCCESS, t0 + DAY + 1).await;

    let (since, until) = (t0 - 1, now_ms() + 1);
    let all = db
        .get_funnel_report_window_for_variant(since, until, None)
        .await
        .unwrap();
    let launched = all["steps"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "funnel.app_launched")
        .unwrap()["installs"]
        .clone();
    assert_eq!(launched, serde_json::json!(3));
    assert_eq!(
        all["metrics"]["first_success_24h"]["matured"],
        serde_json::json!(3)
    );
    assert_eq!(
        all["metrics"]["first_success_24h"]["reached"],
        serde_json::json!(1)
    );

    let c = db
        .get_funnel_report_window_for_variant(since, until, Some(CONTROL))
        .await
        .unwrap();
    assert_eq!(c["variant"], serde_json::json!(CONTROL));
    let m = &c["metrics"];
    assert_eq!(
        m["first_chat_attempt_per_app_launched"],
        serde_json::json!(1)
    );
    assert_eq!(
        m["chat_blocked_on_gateway_per_first_chat_attempt"],
        serde_json::json!(0.5)
    );
    assert_eq!(
        m["onboarding_completed_per_app_launched"],
        serde_json::json!(0.5)
    );
    assert_eq!(m["first_success_24h"]["rate"], serde_json::json!(0.5));

    let x = db
        .get_funnel_report_window_for_variant(since, until, Some(TEST))
        .await
        .unwrap();
    assert_eq!(
        x["metrics"]["first_success_24h"]["launched"],
        serde_json::json!(1)
    );
    assert_eq!(
        x["metrics"]["first_success_24h"]["reached"],
        serde_json::json!(0)
    );
    cleanup(&p);
}

#[tokio::test]
async fn report_routes_take_a_variant_filter() {
    let (db, p) = fresh().await;
    let c1 = install_in(CONTROL, "rc");
    let x1 = install_in(TEST, "rx");
    for iid in [&c1, &x1] {
        db.record_ping(&apex_analytics::db::Ping {
            install_id: iid.clone(),
            version: None,
        })
        .await
        .unwrap();
    }
    let app = build_app(state_for(db.clone()));
    let admin = format!("Bearer {ADMIN_KEY}");

    let get = |uri: String| req_get(&uri, &[("authorization", admin.as_str())]);
    let resp = app
        .clone()
        .oneshot(get("/api/stats/retention?variant=test".into()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let r = body_json(resp).await;
    assert_eq!(r["variant"], serde_json::json!(TEST));
    let size: i64 = r["cohorts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["size"].as_i64().unwrap())
        .sum();
    assert_eq!(size, 1);

    let resp = app
        .clone()
        .oneshot(get("/api/stats/activation?variant=control".into()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["variant"], serde_json::json!(CONTROL));

    let resp = app
        .clone()
        .oneshot(get("/api/stats/funnel?variant=test&hours=48".into()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app
        .oneshot(get("/api/stats/funnel?variant=b".into()))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    cleanup(&p);
}
