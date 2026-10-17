//! Growth-chart merge: admin-gated downloads/repos routes + daily deltas.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use dottie_analytics::app::{build_app, AppState, VersionConfig};
use dottie_analytics::db::Db;
use dottie_analytics::github;
use http_body_util::BodyExt;
use tower::ServiceExt;

const ADMIN_KEY: &str = "test-admin-growth-key-000";

static SEQ: AtomicU64 = AtomicU64::new(0);

fn temp_db() -> String {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!("dottie_growth_{}_{}.db", std::process::id(), n));
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

async fn app() -> (axum::Router, String) {
    let p = temp_db();
    let db = Db::connect(&p, HashSet::new(), HashSet::new())
        .await
        .expect("db");
    let state = AppState::new(
        db,
        ADMIN_KEY.into(),
        VersionConfig {
            latest: "1".into(),
            min: "1".into(),
            download_url: "https://example.invalid".into(),

            default_provider: "dottiepro".into(),


            provider_models: Vec::new(),


            vision_model: "grok-4.3".into(),


            local_model: "ggml-org/gemma-4-E4B-it-GGUF".into(),


            local_model_high_mem: "ggml-org/gemma-4-12B-it-GGUF".into(),


            stt_model_url_base: "https://example.invalid".into(),
        },
    );
    (build_app(state), p)
}

async fn body_json(resp: axum::response::Response) -> (StatusCode, serde_json::Value) {
    let status = resp.status();
    let bytes = resp.into_body().collect().await.expect("body").to_bytes();
    let v = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    (status, v)
}

#[tokio::test]
async fn downloads_404_without_admin() {
    let (app, path) = app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/downloads/repos")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    cleanup(&path);
    // Same as other report APIs: missing key looks like a nonexistent route.
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn repos_list_and_seed_empty() {
    let (app, path) = app().await;
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/downloads/repos")
                .header("authorization", format!("Bearer {ADMIN_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, v) = body_json(resp).await;
    cleanup(&path);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["repos"], serde_json::json!([]));
}

#[tokio::test]
async fn insert_snapshot_and_latest() {
    let p = temp_db();
    let db = Db::connect(&p, HashSet::new(), HashSet::new())
        .await
        .expect("db");
    sqlx::query("INSERT INTO github_repos (repo) VALUES ('acme/app')")
        .execute(db.pool())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO github_downloads (repo, date, tag, download_count) VALUES ('acme/app', '2026-01-01', 'v1', 10)",
    )
    .execute(db.pool())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO github_downloads (repo, date, tag, download_count) VALUES ('acme/app', '2026-01-02', 'v1', 15)",
    )
    .execute(db.pool())
    .await
    .unwrap();

    let latest = github::downloads_latest(&db, Some("acme/app")).await.unwrap();
    assert_eq!(latest["date"], "2026-01-02");
    assert_eq!(latest["total"], 15);

    let daily = github::downloads_daily(&db, Some("acme/app")).await.unwrap();
    let arr = daily.as_array().unwrap();
    assert_eq!(arr.len(), 1);
    assert_eq!(arr[0]["total"], 5);

    cleanup(&p);
}

/// Rolling WoW rate: 15 daily star snapshots at +10/day from 100 → each point
/// compares to 7 days earlier, so the first point lands on day 8 and the rate
/// decays as the base grows. Also asserts the gap fallback (a missing day-7
/// snapshot falls back to the latest one at-or-before it).
#[tokio::test]
async fn wow_series_rates_and_gap_fallback() {
    let p = temp_db();
    let db = Db::connect(&p, HashSet::new(), HashSet::new())
        .await
        .expect("db");
    for i in 0..15i64 {
        let date = format!("2026-03-{:02}", i + 1);
        sqlx::query(
            "INSERT INTO github_metrics (repo, date, metric, count, uniques) VALUES ('acme/app', ?, 'stars', ?, 0)",
        )
        .bind(&date)
        .bind(100 + 10 * i)
        .execute(db.pool())
        .await
        .unwrap();
    }

    let v = github::wow_series(&db, "stars", Some("acme/app")).await.unwrap();
    let pts = v["points"].as_array().unwrap();
    // 15 days of history, 7-day lookback → 8 comparable points (day 8..15).
    assert_eq!(pts.len(), 8);
    assert_eq!(pts[0]["date"], "2026-03-08");
    assert_eq!(pts[0]["prev_date"], "2026-03-01");
    assert_eq!(pts[0]["prev_value"], 100);
    assert_eq!(pts[0]["value"], 170);
    assert_eq!(pts[0]["rate"], 70.0);
    // Base grows, same +70/week → smaller rate.
    assert_eq!(v["latest"]["date"], "2026-03-15");
    assert_eq!(v["latest"]["rate"], 41.18);

    // Drop the exact 7-days-back row for 2026-03-15 (2026-03-08) → falls back to 03-07.
    sqlx::query("DELETE FROM github_metrics WHERE repo = 'acme/app' AND date = '2026-03-08'")
        .execute(db.pool())
        .await
        .unwrap();
    let v = github::wow_series(&db, "stars", Some("acme/app")).await.unwrap();
    assert_eq!(v["latest"]["date"], "2026-03-15");
    assert_eq!(v["latest"]["prev_date"], "2026-03-07");
    assert_eq!(v["latest"]["prev_value"], 160);
    assert_eq!(v["latest"]["value"], 240);
    assert_eq!(v["latest"]["rate"], 50.0);

    cleanup(&p);
}

/// With no `repo` the answer is one row per project, not a cross-repo total.
/// Two repos growing at different rates must stay separate — blending them
/// produces a number that describes neither.
#[tokio::test]
async fn wow_by_repo_keeps_projects_separate() {
    let p = temp_db();
    let db = Db::connect(&p, HashSet::new(), HashSet::new())
        .await
        .expect("db");
    // fast: 100 → 200 over 15 days (+100%/wk early). slow: 1000 → 1014 (+~1%/wk).
    for (repo, base, step) in [("acme/fast", 100i64, 10i64), ("acme/slow", 1000, 1)] {
        sqlx::query("INSERT INTO github_repos (repo) VALUES (?)")
            .bind(repo)
            .execute(db.pool())
            .await
            .unwrap();
        for i in 0..15i64 {
            sqlx::query(
                "INSERT INTO github_metrics (repo, date, metric, count, uniques) VALUES (?, ?, 'stars', ?, 0)",
            )
            .bind(repo)
            .bind(format!("2026-03-{:02}", i + 1))
            .bind(base + step * i)
            .execute(db.pool())
            .await
            .unwrap();
        }
    }

    let v = github::wow_series_by_repo(&db, "stars").await.unwrap();
    let rows = v["repos"].as_array().unwrap();
    assert_eq!(rows.len(), 2, "one row per project");
    // Sorted fastest first, and each rate is that repo's own — 70/170 vs 7/1007.
    assert_eq!(rows[0]["repo"], "acme/fast");
    assert_eq!(rows[0]["rate"], 41.18);
    assert_eq!(rows[1]["repo"], "acme/slow");
    assert_eq!(rows[1]["rate"], 0.7);
    // The blended figure the old endpoint returned would have been neither.
    let blended = github::wow_series(&db, "stars", None).await.unwrap();
    assert_ne!(blended["latest"]["rate"], rows[0]["rate"]);
    assert_ne!(blended["latest"]["rate"], rows[1]["rate"]);
    cleanup(&p);
}

/// Under 8 days of history there is no baseline, so the series is empty rather
/// than guessing a rate from a partial week.
#[tokio::test]
async fn wow_series_empty_without_a_week_of_history() {
    let p = temp_db();
    let db = Db::connect(&p, HashSet::new(), HashSet::new())
        .await
        .expect("db");
    for i in 0..5i64 {
        sqlx::query(
            "INSERT INTO github_metrics (repo, date, metric, count, uniques) VALUES ('acme/app', ?, 'stars', ?, 0)",
        )
        .bind(format!("2026-04-{:02}", i + 1))
        .bind(50 + i)
        .execute(db.pool())
        .await
        .unwrap();
    }
    let v = github::wow_series(&db, "stars", Some("acme/app")).await.unwrap();
    assert_eq!(v["points"].as_array().unwrap().len(), 0);
    assert!(v["latest"].is_null());
    cleanup(&p);
}

/// The endpoint is admin-gated like every other report route (404 without a key)
/// and rejects a metric it cannot rate.
#[tokio::test]
async fn wow_endpoint_gating_and_validation() {
    let (app, path) = app().await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/metrics/wow?metric=stars")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/metrics/wow?metric=bogus")
                .header("authorization", format!("Bearer {ADMIN_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // `downloads` is a valid rate metric even though it lives in its own table.
    let resp = app
        .oneshot(
            Request::builder()
                .uri("/api/downloads/wow")
                .header("authorization", format!("Bearer {ADMIN_KEY}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let (status, v) = body_json(resp).await;
    cleanup(&path);
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["metric"], "downloads");
    // No `repo` → per-project rows, not a blended total across repos.
    assert_eq!(v["repos"], serde_json::json!([]));
    assert!(v["points"].is_null(), "series only when a repo is named");
}
