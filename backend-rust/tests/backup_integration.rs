//! GET /api/ops/db/snapshot: admin-gated, returns a complete, openable SQLite copy
//! of the live DB, and leaves no temp file behind.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use dottie_analytics::app::{build_app, AppState, VersionConfig};
use dottie_analytics::db::Db;
use sqlx::Row;
use tower::ServiceExt; // for `oneshot`

const ADMIN_KEY: &str = "test-admin-key-0000000000";

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_path(tag: &str) -> String {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!("dottie_backup_{tag}_{}_{}.db", std::process::id(), n));
    path.to_string_lossy().into_owned()
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

fn backup_req(auth: Option<&str>) -> Request<Body> {
    let mut b = Request::builder()
        .method("GET")
        .uri("/api/ops/db/snapshot")
        .header("x-forwarded-for", "203.0.113.7");
    if let Some(k) = auth {
        b = b.header("authorization", format!("Bearer {k}"));
    }
    b.body(Body::empty()).unwrap()
}

fn backup_files() -> usize {
    std::fs::read_dir(std::env::temp_dir())
        .map(|d| {
            d.filter_map(Result::ok)
                .filter(|e| {
                    e.file_name()
                        .to_string_lossy()
                        .starts_with("dottie-analytics-backup-")
                })
                .count()
        })
        .unwrap_or(0)
}

#[tokio::test]
async fn backup_404_without_or_with_wrong_key() {
    let p = temp_path("auth");
    let db = Db::connect(&p, HashSet::new(), HashSet::new()).await.unwrap();
    let app = build_app(state_for(db));

    let resp = app.clone().oneshot(backup_req(None)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = app.oneshot(backup_req(Some("wrong-key"))).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    cleanup(&p);
}

#[tokio::test]
async fn backup_streams_a_complete_sqlite_copy() {
    let p = temp_path("src");
    let db = Db::connect(&p, HashSet::new(), HashSet::new()).await.unwrap();
    sqlx::query("CREATE TABLE backup_probe (v TEXT)")
        .execute(db.pool())
        .await
        .unwrap();
    for i in 0..500 {
        sqlx::query("INSERT INTO backup_probe (v) VALUES (?)")
            .bind(format!("row-{i}"))
            .execute(db.pool())
            .await
            .unwrap();
    }
    let migrations: i64 = sqlx::query("SELECT count(*) AS n FROM _sqlx_migrations")
        .fetch_one(db.pool())
        .await
        .unwrap()
        .get("n");

    let before = backup_files();
    let resp = build_app(state_for(db))
        .oneshot(backup_req(Some(ADMIN_KEY)))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let declared: usize = resp.headers()["content-length"]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(bytes.len(), declared);
    assert!(bytes.starts_with(b"SQLite format 3\0"));
    assert_eq!(backup_files(), before, "temp copy must be unlinked");

    let out = temp_path("out");
    std::fs::write(&out, &bytes).unwrap();
    let copy = sqlx::SqlitePool::connect(&format!("sqlite://{out}"))
        .await
        .unwrap();
    let ok: String = sqlx::query("PRAGMA integrity_check")
        .fetch_one(&copy)
        .await
        .unwrap()
        .get(0);
    assert_eq!(ok, "ok");
    let rows: i64 = sqlx::query("SELECT count(*) AS n FROM backup_probe")
        .fetch_one(&copy)
        .await
        .unwrap()
        .get("n");
    assert_eq!(rows, 500);
    let copied: i64 = sqlx::query("SELECT count(*) AS n FROM _sqlx_migrations")
        .fetch_one(&copy)
        .await
        .unwrap()
        .get("n");
    assert_eq!(copied, migrations);
    copy.close().await;
    cleanup(&out);
    cleanup(&p);
}
