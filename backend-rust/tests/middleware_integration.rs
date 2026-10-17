//! Integration tests for the A3 middleware against the real axum app built by
//! `build_app`, driven via `tower::ServiceExt::oneshot` (no network). They pin
//! the externally-observable contract: 404-empty rejects (not 401/403), the
//! valid-bearer allow, the retired shared-key 404, the app-wide `X-Robots-Tag` header,
//! the 4 KiB body cap → 413, and the route-scoped landing CORS origin check.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use dottie_analytics::app::{build_app, AppState, VersionConfig};
use dottie_analytics::db::{Db, RegisterUser};
use tower::ServiceExt; // for `oneshot`

const ADMIN_KEY: &str = "test-admin-key-0000000000";

const LANDING_ALLOWED_ORIGIN: &str = "https://www.example.com";

/// A minimal VALID `/api/events` body — install_id + name present, so the real
/// A4 handler reaches a 200 once the guard passes. (The A3 guard tests used an
/// empty body against the old stub; the real handler validates, so they now send
/// this to isolate guard behavior from body validation.)
const VALID_EVENTS_BODY: &str = r#"{"install_id":"i-guard","name":"app-launched"}"#;

/// Per-test unique temp DB path so parallel tests never share a file.
static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_db_path() -> String {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "dottie_axum_it_{}_{}.db",
        std::process::id(),
        n
    ));
    let p = path.to_string_lossy().into_owned();
    for f in [p.clone(), format!("{p}-wal"), format!("{p}-shm")] {
        let _ = std::fs::remove_file(&f);
    }
    p
}

async fn fresh_db() -> (Db, String) {
    let p = temp_db_path();
    let db = Db::connect(&p, HashSet::new(), HashSet::new())
        .await
        .expect("connect db");
    (db, p)
}

fn cleanup(p: &str) {
    for f in [p.to_string(), format!("{p}-wal"), format!("{p}-shm")] {
        let _ = std::fs::remove_file(&f);
    }
}

fn state_for(db: Db) -> AppState {
    AppState::new(
        db,
        ADMIN_KEY.to_string(),
        VersionConfig {
            latest: "7.99".into(),
            min: "7.99".into(),
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

// ---- desktop_bearer_guard -------------------------------------------------

#[tokio::test]
async fn desktop_guard_404_with_no_key() {
    let (db, p) = fresh_db().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/events")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "no key -> 404");
    cleanup(&p);
}

#[tokio::test]
async fn desktop_guard_404_with_wrong_key() {
    let (db, p) = fresh_db().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/events")
                .header("x-dottie-client", "not-the-real-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "wrong key -> 404");
    cleanup(&p);
}

#[tokio::test]
async fn desktop_guard_404_with_retired_shared_key() {
    // The shared `X-Dottie-Client` key is retired: any value, even a valid
    // body, is a 404. Only a per-user bearer admits desktop ingest.
    let (db, p) = fresh_db().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/events")
                .header("x-dottie-client", "test-desktop-key-00000000")
                .header("content-type", "application/json")
                .body(Body::from(VALID_EVENTS_BODY))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "retired shared key -> 404");
    cleanup(&p);
}

#[tokio::test]
async fn desktop_guard_200_with_valid_bearer() {
    let (db, p) = fresh_db().await;
    let token = db
        .register_user(&RegisterUser {
            name: "Carol".into(),
            email: "carol@example.com".into(),
            install_id: "i-carol".into(),
            ..Default::default()
        })
        .await
        .expect("seed user");
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/events")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(VALID_EVENTS_BODY))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "valid bearer -> 200");
    cleanup(&p);
}

#[tokio::test]
async fn desktop_guard_404_with_bogus_bearer() {
    let (db, p) = fresh_db().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/events")
                .header(
                    "authorization",
                    "Bearer 0000000000000000000000000000000000000000000000000000000000000000",
                )
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "bogus bearer -> 404");
    cleanup(&p);
}

// ---- admin_key_guard ----------------------------------------------------

#[tokio::test]
async fn admin_guard_404_without_key() {
    let (db, p) = fresh_db().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/stats")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "no admin key -> 404");
    cleanup(&p);
}

#[tokio::test]
async fn admin_guard_200_with_key() {
    let (db, p) = fresh_db().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/stats")
                .header("authorization", format!("Bearer {ADMIN_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "correct admin key -> 200");
    cleanup(&p);
}

#[tokio::test]
async fn admin_guard_rejects_desktop_header() {
    // The retired desktop shared-key header must NOT satisfy the admin guard.
    let (db, p) = fresh_db().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/stats")
                .header("x-dottie-client", ADMIN_KEY)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    cleanup(&p);
}

// ---- report_key_guard (read-only key) -----------------------------------

const READ_KEY: &str = "test-read-key-00000000000";

fn state_with_read(db: Db) -> AppState {
    state_for(db).with_read_key(READ_KEY.to_string())
}

#[tokio::test]
async fn read_key_allows_report_get() {
    // The read-only key satisfies a stats GET.
    let (db, p) = fresh_db().await;
    let app = build_app(state_with_read(db));
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/stats")
                .header("authorization", format!("Bearer {READ_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "read key -> 200 on GET");
    cleanup(&p);
}

#[tokio::test]
async fn read_key_rejected_on_write() {
    // The same read-only key must NOT unlock a POST on the report router.
    let (db, p) = fresh_db().await;
    let app = build_app(state_with_read(db));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/sites")
                .header("authorization", format!("Bearer {READ_KEY}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"name":"x","domain":"x.test"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "read key -> 404 on POST");
    cleanup(&p);
}

async fn get_status(app: axum::Router, uri: &str) -> StatusCode {
    app.oneshot(
        Request::builder()
            .method("GET")
            .uri(uri)
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap()
    .status()
}

#[tokio::test]
async fn report_query_key_allows_md() {
    // ?key=<read_key> on the report composite → 200 (browser-friendly, no header).
    let (db, p) = fresh_db().await;
    let app = build_app(state_with_read(db));
    let s = get_status(app, &format!("/api/report/daily?format=md&key={READ_KEY}")).await;
    assert_eq!(s, StatusCode::OK, "query read key -> 200 on report");
    cleanup(&p);
}

#[tokio::test]
async fn report_query_key_wrong_404() {
    let (db, p) = fresh_db().await;
    let app = build_app(state_with_read(db));
    let s = get_status(app, "/api/report/daily?key=nope").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "wrong query key -> 404");
    cleanup(&p);
}

#[tokio::test]
async fn report_query_rejects_admin_key() {
    // The query path is read_key-only — the admin key must NOT work via ?key=.
    let (db, p) = fresh_db().await;
    let app = build_app(state_with_read(db));
    let s = get_status(app, &format!("/api/report/daily?key={ADMIN_KEY}")).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "admin key via query -> 404");
    cleanup(&p);
}

#[tokio::test]
async fn report_query_key_not_honored_elsewhere() {
    // The ?key= query path is scoped to the report composites only; a stats GET
    // still requires a header key.
    let (db, p) = fresh_db().await;
    let app = build_app(state_with_read(db));
    let s = get_status(app, &format!("/api/stats?key={READ_KEY}")).await;
    assert_eq!(s, StatusCode::NOT_FOUND, "query key not honored on /api/stats");
    cleanup(&p);
}

#[tokio::test]
async fn read_key_disabled_when_empty() {
    // With no read_key configured, the read key value must not authenticate.
    let (db, p) = fresh_db().await;
    let app = build_app(state_for(db)); // read_key == ""
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/stats")
                .header("authorization", format!("Bearer {READ_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND, "unset read key -> 404");
    cleanup(&p);
}

// ---- X-Robots-Tag on every response -------------------------------------

#[tokio::test]
async fn x_robots_tag_on_every_response() {
    let (db, p) = fresh_db().await;
    let app = build_app(state_for(db));
    // Health (200), a guarded reject (404), and `/` (404) must ALL carry it.
    for (method, uri) in [("GET", "/health"), ("POST", "/api/events"), ("GET", "/")] {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let hdr = resp
            .headers()
            .get("x-robots-tag")
            .and_then(|v| v.to_str().ok());
        assert_eq!(
            hdr,
            Some("noindex, nofollow, noarchive"),
            "{method} {uri} must carry X-Robots-Tag"
        );
    }
    cleanup(&p);
}

// ---- body cap -> 413 ----------------------------------------------------

#[tokio::test]
async fn oversized_body_returns_413() {
    let (db, p) = fresh_db().await;
    let app = build_app(state_for(db));
    // 4097 bytes — one over the 4096 cap. Send to the landing route (a POST
    // body route) with the allowed origin so only the size triggers the reject.
    let big = vec![b'x'; 4097];
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/landing-visit")
                .header("origin", LANDING_ALLOWED_ORIGIN)
                .header("content-type", "application/json")
                .body(Body::from(big))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::PAYLOAD_TOO_LARGE,
        "over-cap body -> 413"
    );
    cleanup(&p);
}

#[tokio::test]
async fn at_limit_body_is_not_413() {
    let (db, p) = fresh_db().await;
    let app = build_app(state_for(db));
    // Exactly 4096 bytes is allowed (real handler returns 200). Build a VALID
    // landing-visit JSON body padded to exactly 4096 bytes via a long utm field,
    // so only the body size is under test — not JSON parsing or iid validation.
    let prefix = r#"{"iid":"i-cap","utm_term":""#;
    let suffix = r#""}"#;
    let pad = 4096 - prefix.len() - suffix.len();
    let ok = format!("{prefix}{}{suffix}", "a".repeat(pad));
    assert_eq!(ok.len(), 4096, "body is exactly at the cap");
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/landing-visit")
                .header("origin", LANDING_ALLOWED_ORIGIN)
                .header("content-type", "application/json")
                .body(Body::from(ok))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_ne!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(resp.status(), StatusCode::OK, "at-cap body passes through");
    cleanup(&p);
}

// ---- landing CORS origin check ------------------------------------------

#[tokio::test]
async fn landing_cors_allows_correct_origin() {
    let (db, p) = fresh_db().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/landing-visit")
                .header("origin", LANDING_ALLOWED_ORIGIN)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"iid":"i-cors-ok"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "allowed origin passes");
    let acao = resp
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok());
    assert_eq!(
        acao,
        Some(LANDING_ALLOWED_ORIGIN),
        "ACAO echoes the allowed origin"
    );
    cleanup(&p);
}

#[tokio::test]
async fn landing_refused_when_origin_unset() {
    // No LANDING_ALLOWED_ORIGIN configured → every landing call is refused on
    // both routes, including an empty or absent Origin header.
    let (db, p) = fresh_db().await;
    let app = build_app(state_for(db).with_landing_origin(String::new()));
    for uri in ["/api/landing-visit", "/api/landing-event"] {
        for method in ["POST", "OPTIONS"] {
            for origin in [Some(LANDING_ALLOWED_ORIGIN), Some(""), None] {
                let mut req = Request::builder()
                    .method(method)
                    .uri(uri)
                    .header("content-type", "application/json");
                if let Some(o) = origin {
                    req = req.header("origin", o);
                }
                let resp = app
                    .clone()
                    .oneshot(req.body(Body::from("{}")).unwrap())
                    .await
                    .unwrap();
                assert_eq!(resp.status(), StatusCode::FORBIDDEN, "{method} {uri} origin={origin:?}");
            }
        }
    }
    cleanup(&p);
}

#[tokio::test]
async fn landing_origin_config_is_normalized() {
    // A configured origin with a trailing slash or capitals still matches the
    // lowercase Origin a browser sends; a missing Origin is still refused.
    let (db, p) = fresh_db().await;
    let configured = dottie_analytics::app::normalize_origin(" HTTPS://WWW.Example.com/ ");
    assert_eq!(configured, LANDING_ALLOWED_ORIGIN);
    let app = build_app(state_for(db).with_landing_origin(configured));
    let preflight = |origin: Option<&str>| {
        let mut req = Request::builder().method("OPTIONS").uri("/api/landing-event");
        if let Some(o) = origin {
            req = req.header("origin", o);
        }
        req.body(Body::empty()).unwrap()
    };
    let ok = app.clone().oneshot(preflight(Some(LANDING_ALLOWED_ORIGIN))).await.unwrap();
    assert_eq!(ok.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        ok.headers().get("access-control-allow-origin").unwrap(),
        LANDING_ALLOWED_ORIGIN
    );
    let none = app.oneshot(preflight(None)).await.unwrap();
    assert_eq!(none.status(), StatusCode::FORBIDDEN);
    cleanup(&p);
}

#[tokio::test]
async fn landing_cors_omits_acao_for_wrong_origin() {
    // A disallowed origin gets no Access-Control-Allow-Origin header, so a
    // browser blocks the response; the handler also refuses it server-side.
    let (db, p) = fresh_db().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/landing-visit")
                .header("origin", "https://evil.example.com")
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await
        .unwrap();
    let acao = resp
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    assert!(
        acao.is_none(),
        "disallowed origin must NOT receive an ACAO header, got {acao:?}"
    );
    cleanup(&p);
}

#[tokio::test]
async fn landing_cors_preflight_allowed_origin() {
    // An OPTIONS preflight from the allowed origin gets the CORS response headers.
    let (db, p) = fresh_db().await;
    let app = build_app(state_for(db));
    let resp = app
        .oneshot(
            Request::builder()
                .method("OPTIONS")
                .uri("/api/landing-visit")
                .header("origin", LANDING_ALLOWED_ORIGIN)
                .header("access-control-request-method", "POST")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let acao = resp
        .headers()
        .get("access-control-allow-origin")
        .and_then(|v| v.to_str().ok());
    assert_eq!(acao, Some(LANDING_ALLOWED_ORIGIN), "preflight echoes origin");
    cleanup(&p);
}
