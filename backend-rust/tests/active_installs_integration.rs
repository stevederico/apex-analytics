//! DAU/WAU/MAU, active users, retention and lifecycle count an install as
//! active from ANY of: a `pings` row, an `app-launched` event (desktop
//! 2026.10.10+ stopped pinging), or a Pro relay call (`pro_calls` / `pro_usage`,
//! mapped to `users.install_id`, or `user:<id>` when the install is unlinked).

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use apex_analytics::db::Db;
use serde_json::Value;

const DAY: i64 = 24 * 60 * 60 * 1000;
const HOUR: i64 = 60 * 60 * 1000;

static SEQ: AtomicU64 = AtomicU64::new(0);

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

async fn fresh() -> (Db, String) {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let mut path = std::env::temp_dir();
    path.push(format!("dottie_active_{}_{}.db", std::process::id(), n));
    let p = path.to_string_lossy().into_owned();
    cleanup(&p);
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

async fn exec(db: &Db, sql: &str) {
    sqlx::query(sql).execute(db.pool()).await.expect(sql);
}

async fn ping(db: &Db, iid: &str, at: i64) {
    exec(db, &format!("INSERT INTO pings (install_id, version, created_at) VALUES ('{iid}', '1', {at})")).await;
}

async fn event(db: &Db, iid: &str, name: &str, at: i64) {
    exec(
        db,
        &format!(
            "INSERT INTO events (install_id, name, props, version, created_at) \
             VALUES ('{iid}', '{name}', '{{\"macos\":\"15.1\",\"arch\":\"arm64\"}}', '2026.10.10', {at})"
        ),
    )
    .await;
}

async fn user(db: &Db, id: i64, iid: &str) {
    exec(
        db,
        &format!(
            "INSERT INTO users (id, name, email, install_id, created_at, updated_at) \
             VALUES ({id}, 'u{id}', 'u{id}@example.com', '{iid}', 0, 0)"
        ),
    )
    .await;
}

async fn pro_call(db: &Db, user_id: i64, at: i64) {
    exec(
        db,
        &format!(
            "INSERT INTO pro_calls (user_id, route, outcome, created_at) VALUES ({user_id}, 'chat', 'ok', {at})"
        ),
    )
    .await;
}

/// Seeds, relative to `now`:
///   - A: ping 10d ago, `app-launched` 9d ago (D1 return), ping 1h ago
///   - B: `app-launched` only, 10d and 9d ago; a `chat-sent` 1h ago (not a launch)
///   - user 1 (no install): Pro calls 10d, 9d and 2h ago → `user:1`
///   - user 2 (install C): Pro call 3h ago + ping 1h ago → one install
///   - user 3 (no install): `pro_usage` row for today only → `user:3`
async fn seed(db: &Db, now: i64) {
    let first = now - 10 * DAY;
    let d1 = first + DAY + HOUR;

    ping(db, "A", first).await;
    event(db, "A", "app-launched", d1).await;
    ping(db, "A", now - HOUR).await;

    event(db, "B", "app-launched", first).await;
    event(db, "B", "app-launched", d1).await;
    event(db, "B", "chat-sent", now - HOUR).await;

    user(db, 1, "").await;
    pro_call(db, 1, first).await;
    pro_call(db, 1, d1).await;
    pro_call(db, 1, now - 2 * HOUR).await;

    user(db, 2, "C").await;
    pro_call(db, 2, now - 3 * HOUR).await;
    ping(db, "C", now - HOUR).await;

    user(db, 3, "").await;
    exec(
        db,
        "INSERT INTO pro_usage (user_id, day, requests, tokens) VALUES (3, strftime('%Y-%m-%d','now'), 1, 10)",
    )
    .await;
}

#[tokio::test]
async fn dau_wau_mau_union_pings_launches_and_pro() {
    let (db, p) = fresh().await;
    let now = now_ms();
    seed(&db, now).await;

    let s = db.get_stats().await.unwrap();
    // A, C, user:1, user:3. B only launched 9d+ ago; its chat-sent is not a launch.
    assert_eq!(s["dau"], 4, "{s}");
    assert_eq!(s["wau"], 4, "{s}");
    assert_eq!(s["mau"], 5, "{s}");

    let a = db.get_active_users(now - DAY, now + 1).await.unwrap();
    assert_eq!(a["active_users"], 4, "{a}");
    let a = db.get_active_users(now - 30 * DAY, now + 1).await.unwrap();
    assert_eq!(a["active_users"], 5, "{a}");

    cleanup(&p);
}

#[tokio::test]
async fn launch_only_install_counts_without_pings() {
    let (db, p) = fresh().await;
    let now = now_ms();
    event(&db, "LAUNCH-ONLY", "app-launched", now - HOUR).await;
    event(&db, "OTHER", "funnel.app_launched", now - HOUR).await;

    let s = db.get_stats().await.unwrap();
    assert_eq!(s["dau"], 1, "{s}");
    cleanup(&p);
}

#[tokio::test]
async fn retention_counts_launches_and_pro_returns() {
    let (db, p) = fresh().await;
    let now = now_ms();
    seed(&db, now).await;

    let r = db.get_retention(26.0).await.unwrap();
    let cohorts = r["cohorts"].as_array().unwrap();
    // The cohort holding A, B and user:1 (all first seen 10d ago). C and user:3
    // are first seen today, a different ISO week.
    let c: &Value = cohorts
        .iter()
        .find(|c| c["size"] == 3)
        .unwrap_or_else(|| panic!("no 3-member cohort: {r}"));
    assert_eq!(c["d1"]["of"], 3, "{c}");
    assert_eq!(c["d1"]["returned"], 3, "{c}");
    assert_eq!(c["d7"]["returned"], 0, "{c}");

    let w = db.get_retention_window(0, now + DAY).await.unwrap();
    assert_eq!(w["cohorts"], r["cohorts"], "window and weekly retention disagree");

    cleanup(&p);
}

#[tokio::test]
async fn lifecycle_counts_non_ping_activity() {
    let (db, p) = fresh().await;
    let now = now_ms();
    seed(&db, now).await;

    let l = db.get_lifecycle(4.0).await.unwrap();
    let total: i64 = l
        .as_object()
        .and_then(|o| o.values().find_map(|v| v.as_array()))
        .unwrap()
        .iter()
        .map(|w| w["new"].as_i64().unwrap_or(0))
        .sum();
    // Every seeded identity (A, B, user:1, C, user:3) is new in some week.
    assert_eq!(total, 5, "{l}");
    cleanup(&p);
}
