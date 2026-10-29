//! Integration tests for the A4 real handlers (ingest + register + version +
//! landing-visit), driven through the full `build_app` router via
//! `tower::ServiceExt::oneshot` — a faithful port of the JS
//! `backend/__tests__/events.test.js` ingest/auth/CORS/anti-crawler cases, plus
//! the register validation/audit cases and one rate-limit case A3 lacked.
//!
//! Each test gets its OWN temp DB + its OWN `build_app` (so the per-route rate
//! limiter starts empty and parallel tests never share buckets). Bodies are sent
//! as raw JSON strings to exercise the manual `serde_json::from_slice` parsing.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::http::{Request, Response, StatusCode};
use apex_analytics::app::{build_app, AppState, VersionConfig};
use apex_analytics::db::Db;
use tower::ServiceExt; // for `oneshot`

const ADMIN_KEY: &str = "test-admin-key-0000000000";

const LANDING_ALLOWED_ORIGIN: &str = "https://www.example.com";
const LANDING_ORIGIN: &str = LANDING_ALLOWED_ORIGIN;

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_db_path() -> String {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!("dottie_axum_a4_{}_{}.db", std::process::id(), n));
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

// ============================================================================
// /api/events
// ============================================================================

#[tokio::test]
async fn events_200_with_desktop_bearer() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(req_post(
            "/api/events",
            r#"{"install_id":"test-install-1","name":"app-launched","url":"/app","version":"9.27"}"#,
            &[DESKTOP_HDR],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["ok"], serde_json::json!(true));
    cleanup(&p);
}

#[tokio::test]
async fn events_404_without_desktop_bearer() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(req_post(
            "/api/events",
            r#"{"install_id":"test-install-1","name":"foo"}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    cleanup(&p);
}

#[tokio::test]
async fn events_404_with_retired_shared_key() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(req_post(
            "/api/events",
            r#"{"install_id":"i","name":"foo"}"#,
            &[("x-dottie-client", "not-the-real-key")],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    cleanup(&p);
}

#[tokio::test]
async fn events_400_missing_install_id() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(req_post("/api/events", r#"{"name":"foo"}"#, &[DESKTOP_HDR]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(resp).await["error"],
        serde_json::json!("invalid_install_id")
    );
    cleanup(&p);
}

#[tokio::test]
async fn events_400_missing_name() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(req_post(
            "/api/events",
            r#"{"install_id":"i"}"#,
            &[DESKTOP_HDR],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(resp).await["error"],
        serde_json::json!("invalid_name")
    );
    cleanup(&p);
}

#[tokio::test]
async fn events_400_name_over_100_chars() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let name = "x".repeat(101);
    let body = format!(r#"{{"install_id":"i","name":"{name}"}}"#);
    let resp = app
        .oneshot(req_post("/api/events", &body, &[DESKTOP_HDR]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    cleanup(&p);
}

#[tokio::test]
async fn events_400_invalid_json() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(req_post("/api/events", "{not-json", &[DESKTOP_HDR]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(resp).await["error"],
        serde_json::json!("invalid_json")
    );
    cleanup(&p);
}

#[tokio::test]
async fn events_stores_props_as_json() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db.clone()));
    let resp = app
        .oneshot(req_post(
            "/api/events",
            r#"{"install_id":"test-install-props","name":"error.thrown","url":"/app/error","version":"9.27","props":{"file":"Foo.swift","function":"bar()","line":"42"}}"#,
            &[DESKTOP_HDR],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // Assert the props round-tripped into the events.props TEXT column as JSON.
    let row: (String,) =
        sqlx::query_as("SELECT props FROM events WHERE install_id = 'test-install-props'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    let stored: serde_json::Value = serde_json::from_str(&row.0).unwrap();
    assert_eq!(stored["file"], serde_json::json!("Foo.swift"));
    assert_eq!(stored["line"], serde_json::json!("42"));
    cleanup(&p);
}

// ---- dual-auth bearer path --------------------------------------------------

#[tokio::test]
async fn events_200_with_valid_bearer() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db.clone()));
    // Register through the public route to mint a real token.
    let reg = app
        .clone()
        .oneshot(req_post(
            "/api/register",
            r#"{"name":"Carol","email":"carol@test.local","install_id":"i-carol"}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(reg.status(), StatusCode::OK);
    let token = body_json(reg).await["token"].as_str().unwrap().to_string();

    let resp = app
        .oneshot(req_post(
            "/api/events",
            r#"{"install_id":"i-carol","name":"chat-sent"}"#,
            &[("authorization", &format!("Bearer {token}"))],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    cleanup(&p);
}

#[tokio::test]
async fn events_404_with_bogus_bearer() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(req_post(
            "/api/events",
            r#"{"install_id":"i-anon","name":"chat-sent"}"#,
            &[(
                "authorization",
                "Bearer 0000000000000000000000000000000000000000000000000000000000000000",
            )],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    cleanup(&p);
}

// ============================================================================
// /api/ping
// ============================================================================

#[tokio::test]
async fn ping_200_with_desktop_bearer() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(req_post(
            "/api/ping",
            r#"{"install_id":"ping-install","version":"9.27"}"#,
            &[DESKTOP_HDR],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["ok"], serde_json::json!(true));
    cleanup(&p);
}

#[tokio::test]
async fn ping_200_with_jwt_bearer_without_a_prior_row() {
    // After signup moves to Pro, analytics has never seen this token string.
    // Verify + stub the users row from claims so ingest does not 404.
    let (mut db, p) = fresh().await;
    db.jwt_secret = "ingest-secret".into();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let token = apex_analytics::jwt::mint("ingest-secret", 99, "jwt@test.local", Some("dev-9"), now)
        .unwrap();
    let app = build_app(state_for(db.clone()));
    let resp = app
        .oneshot(req_post(
            "/api/ping",
            r#"{"install_id":"jwt-install","version":"2026.9.14"}"#,
            &[("authorization", &format!("Bearer {token}"))],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let row = db.get_user_by_id(99).await.unwrap().expect("stubbed from JWT sub");
    assert_eq!(row.email, "jwt@test.local");
    cleanup(&p);
}

#[tokio::test]
async fn ping_404_with_forged_jwt() {
    let (mut db, p) = fresh().await;
    db.jwt_secret = "ingest-secret".into();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let forged = apex_analytics::jwt::mint("other-secret", 1, "a@b.c", None, now).unwrap();
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(req_post(
            "/api/ping",
            r#"{"install_id":"x","version":"1"}"#,
            &[("authorization", &format!("Bearer {forged}"))],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    cleanup(&p);
}

#[tokio::test]
async fn ping_400_missing_install_id() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(req_post("/api/ping", r#"{"version":"9"}"#, &[DESKTOP_HDR]))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    cleanup(&p);
}

// ============================================================================
// /api/register
// ============================================================================

#[tokio::test]
async fn register_public_upsert_stable_token() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let r1 = app
        .clone()
        .oneshot(req_post(
            "/api/register",
            r#"{"name":"Alice","email":"alice@test.local","install_id":"i1","version":"9.27","os_version":"14.0"}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let b1 = body_json(r1).await;
    assert_eq!(b1["ok"], serde_json::json!(true));
    let t1 = b1["token"].as_str().unwrap().to_string();
    // 64 lowercase hex chars.
    assert_eq!(t1.len(), 64);
    assert!(t1.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));

    // Re-register same email, different name/install/version -> SAME token, 200.
    let r2 = app
        .oneshot(req_post(
            "/api/register",
            r#"{"name":"Alice Two","email":"alice@test.local","install_id":"i2","version":"9.28","os_version":"14.1"}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::OK, "upsert returns 200");
    let t2 = body_json(r2).await["token"].as_str().unwrap().to_string();
    assert_eq!(t2, t1, "re-registering same email keeps token");
    cleanup(&p);
}

#[tokio::test]
async fn ops_registrations_reports_signup_pressure_and_rejections() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let ip = [("cf-connecting-ip", "203.0.113.44")];
    // Two accepted signups from one IP, plus one rejection (bad email).
    for i in 0..2 {
        app.clone()
            .oneshot(req_post(
                "/api/register",
                &format!(r#"{{"name":"Ops {i}","email":"ops{i}@test.local","install_id":"i-ops-{i}"}}"#),
                &ip,
            ))
            .await
            .unwrap();
    }
    app.clone()
        .oneshot(req_post(
            "/api/register",
            r#"{"name":"Bad","email":"nope@test.cm","install_id":"i-bad"}"#,
            &ip,
        ))
        .await
        .unwrap();

    // Admin-gated like every other ops route.
    let resp = app.clone().oneshot(req_get("/api/ops/registrations", &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "no key → 404");

    let resp = app
        .oneshot(req_get(
            "/api/ops/registrations",
            &[("authorization", &format!("Bearer {ADMIN_KEY}"))],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["signups_by_ip"][0]["ip"], "203.0.113.44");
    assert_eq!(body["signups_by_ip"][0]["signups"], 2);
    assert_eq!(
        body["rejected"][0]["ip"], "203.0.113.44",
        "rejections carry the IP that sent them"
    );
    assert!(
        body["rejected"][0]["reason"].as_str().unwrap().starts_with("email_"),
        "reason is recorded: {}",
        body["rejected"][0]["reason"]
    );
    cleanup(&p);
}

#[tokio::test]
async fn register_throttles_per_ip_across_a_restart() {
    // The in-memory rate limiter resets on every deploy, which on the
    // account-minting route handed out a fresh allowance each time. The throttle
    // counts rows, so a brand-new app (a restarted process) still sees them.
    let (db, p) = fresh().await;
    let ip = [("cf-connecting-ip", "203.0.113.5")];
    let app = build_app(state_for(db.clone()));
    for i in 0..10 {
        let resp = app
            .clone()
            .oneshot(req_post(
                "/api/register",
                &format!(
                    r#"{{"name":"User {i}","email":"u{i}@test.local","install_id":"i-{i}"}}"#
                ),
                &ip,
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "signup {i} within the allowance");
    }

    // Fresh router == restarted process, empty rate-limit buckets.
    let restarted = build_app(state_for(db.clone()));
    let resp = restarted
        .clone()
        .oneshot(req_post(
            "/api/register",
            r#"{"name":"Eleven","email":"u10@test.local","install_id":"i-10"}"#,
            &ip,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::TOO_MANY_REQUESTS, "11th from this IP");
    assert_eq!(body_json(resp).await["error"], "too_many_registrations");

    // A different IP is unaffected...
    let resp = restarted
        .clone()
        .oneshot(req_post(
            "/api/register",
            r#"{"name":"Other","email":"other@test.local","install_id":"i-other"}"#,
            &[("cf-connecting-ip", "198.51.100.7")],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "throttle is per IP");

    // ...and so is re-registering an existing email, which updates its row
    // instead of inserting and so never counted against the limit.
    let resp = restarted
        .oneshot(req_post(
            "/api/register",
            r#"{"name":"User 0 again","email":"u0@test.local","install_id":"i-0b"}"#,
            &ip,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "re-registration is not a new account");
    cleanup(&p);
}

#[tokio::test]
async fn register_mints_a_signed_token_and_upgrades_a_legacy_one() {
    // The relay runs as its own service and verifies the bearer offline, so
    // register must hand out a signed token. The same path is the migration:
    // Pro 404s a legacy opaque token, the shipped client re-registers, and
    // walks away with a JWT — no app release, no forced upgrade.
    // Set on the Db, not via env: `PRO_JWT_SECRET` is process-global and
    // would leak into `register_works_without_auth`, which asserts 64-hex.
    let (mut db, p) = fresh().await;
    db.jwt_secret = "integration-secret".into();
    let app = build_app(state_for(db.clone()));

    let resp = app
        .clone()
        .oneshot(req_post(
            "/api/register",
            r#"{"name":"Jay","email":"jay@test.local","install_id":"i-jwt","device_id":"dev-jwt"}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let token = body_json(resp).await["token"].as_str().unwrap().to_string();

    assert!(apex_analytics::jwt::looks_like_jwt(&token), "got {token}");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let claims = apex_analytics::jwt::verify("integration-secret", &token, now).expect("verifies");
    assert_eq!(claims.email, "jay@test.local");
    assert_eq!(claims.device_id.as_deref(), Some("dev-jwt"));

    // The claims must name the real row — Pro trusts `sub` with no lookup.
    let user = db.get_user_by_token(&token).await.unwrap().expect("row");
    assert_eq!(claims.user_id, user.id);

    // Re-registering returns the SAME token: a valid JWT is not rotated, which
    // preserves the old behavior for the common case.
    let resp = app
        .clone()
        .oneshot(req_post(
            "/api/register",
            r#"{"name":"Jay Again","email":"jay@test.local","install_id":"i-jwt-2"}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(body_json(resp).await["token"].as_str().unwrap(), token, "stable");

    // Now simulate an install still holding a pre-migration opaque token.
    sqlx_set_legacy_token(&db, user.id).await;
    let resp = app
        .oneshot(req_post(
            "/api/register",
            r#"{"name":"Jay","email":"jay@test.local","install_id":"i-jwt-3"}"#,
            &[],
        ))
        .await
        .unwrap();
    let upgraded = body_json(resp).await["token"].as_str().unwrap().to_string();
    assert!(
        apex_analytics::jwt::looks_like_jwt(&upgraded),
        "a legacy token is replaced on re-registration — this is the migration"
    );
    assert!(apex_analytics::jwt::verify("integration-secret", &upgraded, now).is_ok());
    cleanup(&p);
}

/// Force a row back to a legacy 64-hex token, the shape every install carried
/// before signed tokens existed.
async fn sqlx_set_legacy_token(db: &apex_analytics::db::Db, user_id: i64) {
    db.set_token_for_test(user_id, &"a".repeat(64)).await.expect("set legacy token");
}

#[tokio::test]
async fn register_works_without_auth() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(req_post(
            "/api/register",
            r#"{"name":"Bob","email":"bob@test.local","install_id":"i-bob","version":"2026.5.1.9"}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let token = body_json(resp).await["token"].as_str().unwrap().to_string();
    assert_eq!(token.len(), 64);
    cleanup(&p);
}

#[tokio::test]
async fn register_400_typo_tld_email() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(req_post(
            "/api/register",
            r#"{"name":"Isaac","email":"real.person@gmail.cm","install_id":"i-typo"}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(resp).await["error"],
        serde_json::json!("invalid_email")
    );
    cleanup(&p);
}

#[tokio::test]
async fn register_400_keyboard_mash_local() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(req_post(
            "/api/register",
            r#"{"name":"Real Name","email":"asdfasdf@gmail.com","install_id":"i-mash"}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(resp).await["error"],
        serde_json::json!("invalid_email")
    );
    cleanup(&p);
}

#[tokio::test]
async fn register_400_keyboard_mash_name() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(req_post(
            "/api/register",
            r#"{"name":"asdfasdf","email":"ok@example.com","install_id":"i-mash-name"}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(resp).await["error"],
        serde_json::json!("invalid_name")
    );
    cleanup(&p);
}

#[tokio::test]
async fn register_400_single_letter_name() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(req_post(
            "/api/register",
            r#"{"name":"a","email":"ok2@example.com","install_id":"i-short-name"}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(resp).await["error"],
        serde_json::json!("invalid_name")
    );
    cleanup(&p);
}

#[tokio::test]
async fn register_logs_rejection_reason() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db.clone()));
    let resp = app
        .oneshot(req_post(
            "/api/register",
            r#"{"name":"Curious","email":"asdfadsf@gmail.cm","install_id":"i-log-check","version":"2026.5.11"}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // Exactly one rejection row, with the .cm-typo reason and the email.
    let n: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM rejected_registrations")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(n.0, 1, "exactly one rejection row");
    let row: (String, String) =
        sqlx::query_as("SELECT email, reason FROM rejected_registrations ORDER BY id DESC LIMIT 1")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(row.0, "asdfadsf@gmail.cm");
    assert_eq!(row.1, "email_typo_tld_cm");
    cleanup(&p);
}

#[tokio::test]
async fn register_accepts_legit_edge_cases() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    // "Jo" (2-char name boundary) + plus-addressing email.
    let resp = app
        .oneshot(req_post(
            "/api/register",
            r#"{"name":"Jo","email":"jo+test123@example.com","install_id":"i-legit-edge"}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    cleanup(&p);
}

// ============================================================================
// /api/version
// ============================================================================

#[tokio::test]
async fn version_is_public() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app.oneshot(req_get("/api/version", &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = body_json(resp).await;
    assert_eq!(body["latest"], serde_json::json!("9.99"));
    assert_eq!(body["minimum_required"], serde_json::json!("9.50"));
    assert_eq!(
        body["download_url"],
        serde_json::json!("https://example.invalid/dl")
    );
    cleanup(&p);
}

// ============================================================================
// /api/landing-visit
// ============================================================================

#[tokio::test]
async fn landing_200_allowed_origin() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(req_post(
            "/api/landing-visit",
            r#"{"iid":"iid-landing-1","path":"/","referrer":"https://google.com/"}"#,
            &[("origin", LANDING_ORIGIN), ("x-forwarded-for", "203.0.113.10")],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let acao = resp
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok());
    assert_eq!(acao, Some(LANDING_ORIGIN), "ACAO set on success");
    assert_eq!(body_json(resp).await["ok"], serde_json::json!(true));
    cleanup(&p);
}

#[tokio::test]
async fn landing_403_wrong_origin() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(req_post(
            "/api/landing-visit",
            r#"{"iid":"iid-landing-2"}"#,
            &[("origin", "https://evil.example.com")],
        ))
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::FORBIDDEN,
        "handler 403s a wrong origin server-side"
    );
    cleanup(&p);
}

#[tokio::test]
async fn landing_400_invalid_iid() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(req_post(
            "/api/landing-visit",
            r#"{"iid":""}"#,
            &[("origin", LANDING_ORIGIN)],
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        body_json(resp).await["error"],
        serde_json::json!("invalid_iid")
    );
    cleanup(&p);
}

#[tokio::test]
async fn landing_options_sets_cors_headers() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/api/landing-visit")
                .header("origin", LANDING_ORIGIN)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let acao = resp
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok());
    assert_eq!(acao, Some(LANDING_ORIGIN));
    cleanup(&p);
}

// ============================================================================
// Anti-crawler
// ============================================================================

#[tokio::test]
async fn root_returns_404() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app.oneshot(req_get("/", &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    cleanup(&p);
}

#[tokio::test]
async fn robots_disallows_crawlers() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    let resp = app.oneshot(req_get("/robots.txt", &[])).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains("Disallow: /"), "robots disallows everything");
    cleanup(&p);
}

#[tokio::test]
async fn x_robots_tag_present_everywhere() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    for req in [
        req_get("/health", &[]),
        req_get("/api/version", &[]),
        req_post("/api/events", "{}", &[]), // 404 reject still carries the header
        req_get("/", &[]),
    ] {
        let resp = app.clone().oneshot(req).await.unwrap();
        let hdr = resp
            .headers()
            .get("x-robots-tag")
            .and_then(|v| v.to_str().ok());
        assert_eq!(hdr, Some("noindex, nofollow, noarchive"));
    }
    cleanup(&p);
}

// ============================================================================
// Rate limit (the case A3 lacked): exceed a route's max via a fixed XFF -> 429.
// ============================================================================

#[tokio::test]
async fn rate_limit_429_after_max() {
    let (db, p) = fresh().await;
    let app = build_app(state_for(db));
    // /api/register has max=10/min. With a FIXED X-Forwarded-For (same bucket
    // key), the 11th request in the window must be rejected with 429. Use a typo
    // email so the handler short-circuits at validation — the rate limiter runs
    // OUTERMOST, so it still counts every request regardless of the 400.
    let ip = "9.9.9.9";
    for i in 1..=10 {
        let resp = app
            .clone()
            .oneshot(req_post(
                "/api/register",
                r#"{"name":"Isaac","email":"real.person@gmail.cm","install_id":"i"}"#,
                &[("x-forwarded-for", ip)],
            ))
            .await
            .unwrap();
        assert_ne!(
            resp.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "request {i} within the limit must not be 429"
        );
    }
    let over = app
        .oneshot(req_post(
            "/api/register",
            r#"{"name":"Isaac","email":"real.person@gmail.cm","install_id":"i"}"#,
            &[("x-forwarded-for", ip)],
        ))
        .await
        .unwrap();
    assert_eq!(
        over.status(),
        StatusCode::TOO_MANY_REQUESTS,
        "11th request over the limit must be 429"
    );
    cleanup(&p);
}
