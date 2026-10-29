//! Growth-chart merge — GitHub release download + stars/forks/clones/views
//! tracking. Ported from `growth-chart/backend/github.ts`.
//!
//! Tables: `github_repos`, `github_downloads`, `github_metrics`.
//! Collectors: snapshot on startup + hourly backfill of today's missing rows.
//! Env: `GITHUB_TOKEN` (traffic metrics), `GITHUB_REPOS` (seed owner/name list),
//! `GITHUB_SITE_LINKS` (repo → site id links).

use serde_json::{json, Value};
use sqlx::Row;

use crate::db::Db;

pub const VALID_METRICS: &[&str] = &["stars", "forks", "clones", "views"];

/// Repo classification: website | repo | mac_app.
pub const VALID_KINDS: &[&str] = &["website", "repo", "mac_app"];

pub fn normalize_kind(raw: Option<&str>) -> Option<&'static str> {
    match raw.map(str::trim).unwrap_or("") {
        "" => Some("repo"),
        "website" | "site" | "web" => Some("website"),
        "repo" | "library" | "oss" => Some("repo"),
        "mac_app" | "mac-app" | "mac" | "app" | "desktop" => Some("mac_app"),
        _ => None,
    }
}

/// Config from env (token optional; repos seed the table when empty).
#[derive(Clone, Debug, Default)]
pub struct GithubConfig {
    pub token: String,
    pub seed_repos: Vec<(String, &'static str)>,
    /// (owner/name, site id or `"native"`) from `GITHUB_SITE_LINKS`.
    pub site_links: Vec<(String, String)>,
}

impl GithubConfig {
    pub fn from_env() -> Self {
        let token = std::env::var("GITHUB_TOKEN").unwrap_or_default();
        let seed = std::env::var("GITHUB_REPOS")
            .or_else(|_| std::env::var("GITHUB_REPO"))
            .unwrap_or_default();
        let seed_repos: Vec<(String, &'static str)> = seed
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| {
                // owner/name or owner/name:kind
                let (name, kind) = match s.split_once(':') {
                    Some((n, k)) => (n.trim(), normalize_kind(Some(k)).unwrap_or("repo")),
                    None => (s, "repo"),
                };
                (name.to_string(), kind)
            })
            .collect();
        let site_links = parse_site_links(&std::env::var("GITHUB_SITE_LINKS").unwrap_or_default());
        Self {
            token,
            seed_repos,
            site_links,
        }
    }

    /// Configured site link for `repo`, if any. Case-insensitive, like GitHub names.
    fn link_for(&self, repo: &str) -> Option<&str> {
        self.site_links
            .iter()
            .find(|(r, _)| r.eq_ignore_ascii_case(repo))
            .map(|(_, s)| s.as_str())
    }
}

/// Parse `owner/name=site_id,...` (site id may be `native`). Blank or
/// malformed entries are skipped.
fn parse_site_links(raw: &str) -> Vec<(String, String)> {
    raw.split(',')
        .filter_map(|s| s.split_once('='))
        .map(|(r, site)| (r.trim().to_string(), site.trim().to_string()))
        .filter(|(r, site)| r.contains('/') && !site.is_empty())
        .collect()
}

fn repo_row_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    let linked: Option<String> = r
        .try_get::<Option<String>, _>("linked_site_id")
        .unwrap_or(None);
    let disabled: Option<String> = r
        .try_get::<Option<String>, _>("disabled_at")
        .unwrap_or(None);
    json!({
        "id": r.get::<i64, _>("id"),
        "repo": r.get::<String, _>("repo"),
        "kind": r.try_get::<String, _>("kind").unwrap_or_else(|_| "repo".into()),
        "linked_site_id": linked,
        "created_at": r.get::<String, _>("created_at"),
        "disabled_at": disabled,
    })
}

/// GitHub answers 404 for a repo that went private or was renamed — a
/// deterministic failure that would otherwise error every collector cycle
/// forever (field 2026-08: three private repos × every snapshot). Disable the
/// row (history kept, collectors skip it); any PATCH to the row re-enables.
/// Returns true when the error was a 404 (caller logs warn instead of error).
async fn disable_repo_on_404(db: &Db, repo: &str, err: &str) -> bool {
    if !err.contains("404") {
        return false;
    }
    let res = sqlx::query(
        "UPDATE github_repos SET disabled_at = datetime('now') \
         WHERE repo = ? AND disabled_at IS NULL",
    )
    .bind(repo)
    .execute(db.pool())
    .await;
    match res {
        Ok(r) if r.rows_affected() > 0 => {
            tracing::warn!(repo, "GitHub 404 (private/renamed) — repo auto-disabled; PATCH it to re-enable");
        }
        Ok(_) => {}
        Err(e) => tracing::error!(repo, error = %e, "auto-disable failed"),
    }
    true
}

fn today_utc() -> String {
    // YYYY-MM-DD UTC without chrono dep.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    // 1970-01-01 + days — use a minimal civil date conversion.
    civil_date(days)
}

/// Days since Unix epoch → YYYY-MM-DD (proleptic Gregorian, UTC).
fn civil_date(mut z: u64) -> String {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    z += 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn github_headers(token: &str, authenticated: bool) -> reqwest::header::HeaderMap {
    let mut h = reqwest::header::HeaderMap::new();
    h.insert(
        reqwest::header::USER_AGENT,
        reqwest::header::HeaderValue::from_static("apex-analytics-growth/1.0"),
    );
    h.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static("application/vnd.github+json"),
    );
    if authenticated && !token.is_empty() {
        if let Ok(v) =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
        {
            h.insert(reqwest::header::AUTHORIZATION, v);
        }
    }
    h
}

/// Seed `github_repos` from config when empty; re-apply known site links.
pub async fn seed_repos(db: &Db, cfg: &GithubConfig) -> Result<(), sqlx::Error> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM github_repos")
        .fetch_one(db.pool())
        .await?;
    if count == 0 && !cfg.seed_repos.is_empty() {
        for (repo, kind) in &cfg.seed_repos {
            let link = cfg.link_for(repo);
            sqlx::query(
                "INSERT OR IGNORE INTO github_repos (repo, kind, linked_site_id) VALUES (?, ?, ?)",
            )
            .bind(repo)
            .bind(*kind)
            .bind(link)
            .execute(db.pool())
            .await?;
        }
        tracing::info!(n = cfg.seed_repos.len(), "seeded github_repos from GITHUB_REPOS");
    }
    // Idempotent links from `GITHUB_SITE_LINKS`; never overwrite a set link.
    for (repo, site) in &cfg.site_links {
        sqlx::query(
            "UPDATE github_repos SET linked_site_id = ? \
             WHERE repo = ? COLLATE NOCASE AND (linked_site_id IS NULL OR linked_site_id = '')",
        )
        .bind(site)
        .bind(repo)
        .execute(db.pool())
        .await?;
    }
    Ok(())
}

pub async fn list_repo_names(db: &Db) -> Result<Vec<String>, sqlx::Error> {
    let rows = sqlx::query(
        "SELECT repo FROM github_repos WHERE disabled_at IS NULL ORDER BY created_at ASC",
    )
    .fetch_all(db.pool())
    .await?;
    Ok(rows
        .into_iter()
        .filter_map(|r| r.try_get::<String, _>("repo").ok())
        .collect())
}

pub async fn list_repos(db: &Db, kind: Option<&str>) -> Result<Value, sqlx::Error> {
    let rows = if let Some(k) = kind {
        sqlx::query(
            "SELECT id, repo, kind, linked_site_id, created_at, disabled_at FROM github_repos \
             WHERE kind = ? ORDER BY created_at ASC",
        )
        .bind(k)
        .fetch_all(db.pool())
        .await?
    } else {
        sqlx::query(
            "SELECT id, repo, kind, linked_site_id, created_at, disabled_at FROM github_repos \
             ORDER BY created_at ASC",
        )
        .fetch_all(db.pool())
        .await?
    };
    let repos: Vec<Value> = rows.iter().map(repo_row_json).collect();
    Ok(json!({ "repos": repos }))
}

pub async fn list_repo_names_json(db: &Db, kind: Option<&str>) -> Result<Value, sqlx::Error> {
    let listed = list_repos(db, kind).await?;
    let names: Vec<String> = listed["repos"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|r| r.get("repo").and_then(Value::as_str).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "repos": names,
        "items": listed["repos"],
    }))
}

pub async fn add_repo(
    db: &Db,
    cfg: &GithubConfig,
    repo: &str,
    kind: Option<&str>,
) -> Result<Value, String> {
    let repo = repo.trim();
    if repo.is_empty() || !repo.contains('/') {
        return Err("Repo must be in \"owner/name\" format".into());
    }
    let kind = normalize_kind(kind).ok_or_else(|| {
        format!(
            "Invalid kind, must be one of: {}",
            VALID_KINDS.join(", ")
        )
    })?;
    let client = reqwest::Client::new();
    let res = client
        .get(format!("https://api.github.com/repos/{repo}"))
        .headers(github_headers(&cfg.token, !cfg.token.is_empty()))
        .send()
        .await
        .map_err(|e| format!("Failed to validate repo on GitHub: {e}"))?;
    if !res.status().is_success() {
        return Err(format!("GitHub repo \"{repo}\" not found"));
    }
    let link = cfg.link_for(repo);
    let result = sqlx::query(
        "INSERT INTO github_repos (repo, kind, linked_site_id) VALUES (?, ?, ?)",
    )
    .bind(repo)
    .bind(kind)
    .bind(link)
    .execute(db.pool())
    .await;
    match result {
        Ok(_) => {}
        Err(e) if e.to_string().contains("UNIQUE") => {
            return Err("Repo already added".into());
        }
        Err(e) => return Err(e.to_string()),
    }
    let row = sqlx::query(
        "SELECT id, repo, kind, linked_site_id, created_at, disabled_at FROM github_repos WHERE repo = ?",
    )
    .bind(repo)
    .fetch_one(db.pool())
    .await
    .map_err(|e| e.to_string())?;
    // Fire-and-forget: downloads + stars/forks/clones/views (then hourly keeps it current).
    spawn_repo_snapshot(db.clone(), cfg.clone(), repo.to_string());
    Ok(repo_row_json(&row))
}

/// Pull whatever GitHub allows right now for one repo (releases + metrics).
/// Used on add/link; the hourly collector re-runs for every tracked repo afterward.
pub async fn snapshot_repo_now(db: &Db, cfg: &GithubConfig, repo: &str) {
    if let Err(e) = fetch_download_snapshot(db, cfg, repo).await {
        tracing::error!(repo, error = %e, "snapshot downloads failed");
    }
    for metric in VALID_METRICS {
        if let Err(e) = fetch_metric(db, cfg, repo, metric).await {
            tracing::error!(repo, metric, error = %e, "snapshot metric failed");
        }
    }
}

fn spawn_repo_snapshot(db: Db, cfg: GithubConfig, repo: String) {
    tokio::spawn(async move {
        snapshot_repo_now(&db, &cfg, &repo).await;
    });
}

/// PATCH kind and/or linked_site_id for an existing row.
/// `linked_site_id`: site uuid, `"native"`, or `""` / null to clear.
pub async fn patch_repo(
    db: &Db,
    id: i64,
    kind: Option<&str>,
    linked_site_id: Option<Option<&str>>,
) -> Result<Option<Value>, String> {
    if kind.is_none() && linked_site_id.is_none() {
        return Err("kind or linked_site_id required".into());
    }
    if let Some(k) = kind {
        let kind = normalize_kind(Some(k)).ok_or_else(|| {
            format!(
                "Invalid kind, must be one of: {}",
                VALID_KINDS.join(", ")
            )
        })?;
        let affected = sqlx::query("UPDATE github_repos SET kind = ? WHERE id = ?")
            .bind(kind)
            .bind(id)
            .execute(db.pool())
            .await
            .map_err(|e| e.to_string())?
            .rows_affected();
        if affected == 0 {
            return Ok(None);
        }
    }
    if let Some(link) = linked_site_id {
        let val = link.filter(|s| !s.is_empty());
        let affected = sqlx::query("UPDATE github_repos SET linked_site_id = ? WHERE id = ?")
            .bind(val)
            .bind(id)
            .execute(db.pool())
            .await
            .map_err(|e| e.to_string())?
            .rows_affected();
        if affected == 0 {
            return Ok(None);
        }
    }
    // Touching a repo is the re-enable path for one auto-disabled after a
    // GitHub 404 (went private/renamed) — clear the flag so collectors resume.
    sqlx::query("UPDATE github_repos SET disabled_at = NULL WHERE id = ?")
        .bind(id)
        .execute(db.pool())
        .await
        .map_err(|e| e.to_string())?;
    let row = sqlx::query(
        "SELECT id, repo, kind, linked_site_id, created_at, disabled_at FROM github_repos WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| e.to_string())?;
    Ok(row.as_ref().map(repo_row_json))
}

/// PATCH kind only (compat).
pub async fn set_repo_kind(db: &Db, id: i64, kind: &str) -> Result<Option<Value>, String> {
    patch_repo(db, id, Some(kind), None).await
}

/// Bulk import download snapshot rows (growth-chart export shape).
/// Each: `{ repo, date, tag, download_count }`. Upserts repos as `repo` kind.
pub async fn import_downloads(db: &Db, rows: &[Value]) -> Result<usize, String> {
    let mut n = 0usize;
    for r in rows {
        let repo = r
            .get("repo")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let date = r.get("date").and_then(Value::as_str).unwrap_or("").trim();
        let tag = r.get("tag").and_then(Value::as_str).unwrap_or("").trim();
        let count = r
            .get("download_count")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if repo.is_empty() || date.is_empty() || tag.is_empty() {
            continue;
        }
        sqlx::query("INSERT OR IGNORE INTO github_repos (repo, kind) VALUES (?, 'repo')")
            .bind(repo)
            .execute(db.pool())
            .await
            .map_err(|e| e.to_string())?;
        sqlx::query(
            "INSERT OR REPLACE INTO github_downloads (repo, date, tag, download_count) VALUES (?, ?, ?, ?)",
        )
        .bind(repo)
        .bind(date)
        .bind(tag)
        .bind(count)
        .execute(db.pool())
        .await
        .map_err(|e| e.to_string())?;
        n += 1;
    }
    Ok(n)
}

/// Bulk import metric rows (growth-chart export). Each: `{ repo, date, metric, count, uniques? }`.
pub async fn import_metrics(db: &Db, rows: &[Value]) -> Result<usize, String> {
    let mut n = 0usize;
    for r in rows {
        let repo = r
            .get("repo")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let date = r.get("date").and_then(Value::as_str).unwrap_or("").trim();
        let metric = r.get("metric").and_then(Value::as_str).unwrap_or("").trim();
        let count = r.get("count").and_then(Value::as_i64).unwrap_or(0);
        let uniques = r.get("uniques").and_then(Value::as_i64).unwrap_or(0);
        if repo.is_empty() || date.is_empty() || !VALID_METRICS.contains(&metric) {
            continue;
        }
        sqlx::query("INSERT OR IGNORE INTO github_repos (repo, kind) VALUES (?, 'repo')")
            .bind(repo)
            .execute(db.pool())
            .await
            .map_err(|e| e.to_string())?;
        sqlx::query(
            "INSERT OR REPLACE INTO github_metrics (repo, date, metric, count, uniques) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(repo)
        .bind(date)
        .bind(metric)
        .bind(count)
        .bind(uniques)
        .execute(db.pool())
        .await
        .map_err(|e| e.to_string())?;
        n += 1;
    }
    Ok(n)
}

pub async fn delete_repo(db: &Db, id: i64) -> Result<Option<String>, sqlx::Error> {
    let row = sqlx::query("SELECT repo FROM github_repos WHERE id = ?")
        .bind(id)
        .fetch_optional(db.pool())
        .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    let repo: String = row.get("repo");
    sqlx::query("DELETE FROM github_repos WHERE id = ?")
        .bind(id)
        .execute(db.pool())
        .await?;
    Ok(Some(repo))
}

async fn has_download_row(db: &Db, repo: &str, date: &str) -> Result<bool, sqlx::Error> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM github_downloads WHERE repo = ? AND date = ?",
    )
    .bind(repo)
    .bind(date)
    .fetch_one(db.pool())
    .await?;
    Ok(n > 0)
}

async fn has_metric_row(
    db: &Db,
    repo: &str,
    date: &str,
    metric: &str,
) -> Result<bool, sqlx::Error> {
    let n: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM github_metrics WHERE repo = ? AND date = ? AND metric = ?",
    )
    .bind(repo)
    .bind(date)
    .bind(metric)
    .fetch_one(db.pool())
    .await?;
    Ok(n > 0)
}

/// Fetch release download counts for one repo; idempotent per (repo, date).
pub async fn fetch_download_snapshot(
    db: &Db,
    cfg: &GithubConfig,
    repo: &str,
) -> Result<Value, String> {
    if repo.is_empty() {
        return Err("No repo provided".into());
    }
    let today = today_utc();
    if has_download_row(db, repo, &today)
        .await
        .map_err(|e| e.to_string())?
    {
        return Ok(json!({ "repo": repo, "date": today, "releases": [] }));
    }
    let client = reqwest::Client::new();
    let res = client
        .get(format!("https://api.github.com/repos/{repo}/releases?per_page=100"))
        .headers(github_headers(&cfg.token, false))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        return Err(format!("GitHub API returned {}", res.status()));
    }
    let releases: Value = res.json().await.map_err(|e| e.to_string())?;
    let arr = releases.as_array().cloned().unwrap_or_default();
    let mut results = Vec::new();
    for rel in arr {
        let tag = rel
            .get("tag_name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let download_count: i64 = rel
            .get("assets")
            .and_then(Value::as_array)
            .map(|assets| {
                assets
                    .iter()
                    .map(|a| a.get("download_count").and_then(Value::as_i64).unwrap_or(0))
                    .sum()
            })
            .unwrap_or(0);
        sqlx::query(
            "INSERT OR REPLACE INTO github_downloads (repo, date, tag, download_count) VALUES (?, ?, ?, ?)",
        )
        .bind(repo)
        .bind(&today)
        .bind(&tag)
        .bind(download_count)
        .execute(db.pool())
        .await
        .map_err(|e| e.to_string())?;
        results.push(json!({ "tag": tag, "download_count": download_count }));
    }
    tracing::info!(repo, date = %today, n = results.len(), "download snapshot saved");
    Ok(json!({ "repo": repo, "date": today, "releases": results }))
}

pub async fn fetch_all_download_snapshots(db: &Db, cfg: &GithubConfig) {
    let repos = match list_repo_names(db).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "list repos failed");
            return;
        }
    };
    if repos.is_empty() {
        tracing::warn!("no github repos configured, skipping download snapshots");
        return;
    }
    for repo in repos {
        if let Err(e) = fetch_download_snapshot(db, cfg, &repo).await {
            if !disable_repo_on_404(db, &repo, &e).await {
                tracing::error!(repo, error = %e, "download snapshot failed");
            }
        }
    }
}

async fn fetch_traffic(db: &Db, cfg: &GithubConfig, repo: &str, metric: &str) -> Result<(), String> {
    if cfg.token.is_empty() {
        tracing::warn!(repo, metric, "GITHUB_TOKEN not set, skipping traffic");
        return Ok(());
    }
    let client = reqwest::Client::new();
    let res = client
        .get(format!("https://api.github.com/repos/{repo}/traffic/{metric}"))
        .headers(github_headers(&cfg.token, true))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        tracing::warn!(repo, metric, status = %res.status(), "traffic API failed");
        return Ok(());
    }
    let data: Value = res.json().await.map_err(|e| e.to_string())?;
    let entries = data
        .get(metric)
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for entry in entries {
        let ts = entry.get("timestamp").and_then(Value::as_str).unwrap_or("");
        let date = if ts.len() >= 10 {
            ts[..10].to_string()
        } else {
            continue;
        };
        let count = entry.get("count").and_then(Value::as_i64).unwrap_or(0);
        let uniques = entry.get("uniques").and_then(Value::as_i64).unwrap_or(0);
        sqlx::query(
            "INSERT OR REPLACE INTO github_metrics (repo, date, metric, count, uniques) VALUES (?, ?, ?, ?, ?)",
        )
        .bind(repo)
        .bind(&date)
        .bind(metric)
        .bind(count)
        .bind(uniques)
        .execute(db.pool())
        .await
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

async fn fetch_count_metric(
    db: &Db,
    cfg: &GithubConfig,
    repo: &str,
    metric: &str,
) -> Result<(), String> {
    let today = today_utc();
    if has_metric_row(db, repo, &today, metric)
        .await
        .map_err(|e| e.to_string())?
    {
        return Ok(());
    }
    let client = reqwest::Client::new();
    let res = client
        .get(format!("https://api.github.com/repos/{repo}"))
        .headers(github_headers(&cfg.token, !cfg.token.is_empty()))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !res.status().is_success() {
        tracing::warn!(repo, metric, status = %res.status(), "repo API failed");
        return Ok(());
    }
    let data: Value = res.json().await.map_err(|e| e.to_string())?;
    let count = if metric == "stars" {
        data.get("stargazers_count").and_then(Value::as_i64).unwrap_or(0)
    } else {
        data.get("forks_count").and_then(Value::as_i64).unwrap_or(0)
    };
    sqlx::query(
        "INSERT OR REPLACE INTO github_metrics (repo, date, metric, count, uniques) VALUES (?, ?, ?, ?, 0)",
    )
    .bind(repo)
    .bind(&today)
    .bind(metric)
    .bind(count)
    .execute(db.pool())
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub async fn fetch_metric(
    db: &Db,
    cfg: &GithubConfig,
    repo: &str,
    metric: &str,
) -> Result<(), String> {
    match metric {
        "clones" | "views" => fetch_traffic(db, cfg, repo, metric).await,
        "stars" | "forks" => fetch_count_metric(db, cfg, repo, metric).await,
        _ => Err(format!("invalid metric: {metric}")),
    }
}

pub async fn fetch_all_metric_snapshots(db: &Db, cfg: &GithubConfig) {
    let repos = match list_repo_names(db).await {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, "list repos failed");
            return;
        }
    };
    for repo in repos {
        for metric in VALID_METRICS {
            if let Err(e) = fetch_metric(db, cfg, &repo, metric).await {
                if disable_repo_on_404(db, &repo, &e).await {
                    break; // remaining metrics would 404 the same way
                }
                tracing::error!(repo, metric, error = %e, "metric snapshot failed");
            }
        }
    }
}

/// Startup + hourly collectors (same behavior as growth-chart).
pub fn start_collectors(db: Db, cfg: GithubConfig) {
    tokio::spawn({
        let db = db.clone();
        let cfg = cfg.clone();
        async move {
            if let Err(e) = seed_repos(&db, &cfg).await {
                tracing::error!(error = %e, "seed github_repos failed");
            }
            fetch_all_download_snapshots(&db, &cfg).await;
            fetch_all_metric_snapshots(&db, &cfg).await;
        }
    });
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(3600));
        interval.tick().await; // skip immediate (startup already ran)
        loop {
            interval.tick().await;
            let today = today_utc();
            let repos = match list_repo_names(&db).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::error!(error = %e, "hourly list repos failed");
                    continue;
                }
            };
            'repos: for repo in repos {
                match has_download_row(&db, &repo, &today).await {
                    Ok(false) => {
                        if let Err(e) = fetch_download_snapshot(&db, &cfg, &repo).await {
                            if disable_repo_on_404(&db, &repo, &e).await {
                                continue 'repos; // metrics would 404 the same way
                            }
                            tracing::error!(repo, error = %e, "hourly download snapshot failed");
                        }
                    }
                    Ok(true) => {}
                    Err(e) => tracing::error!(error = %e, "has_download_row failed"),
                }
                for metric in VALID_METRICS {
                    match has_metric_row(&db, &repo, &today, metric).await {
                        Ok(false) => {
                            if let Err(e) = fetch_metric(&db, &cfg, &repo, metric).await {
                                if disable_repo_on_404(&db, &repo, &e).await {
                                    continue 'repos;
                                }
                                tracing::error!(repo, metric, error = %e, "hourly metric failed");
                            }
                        }
                        Ok(true) => {}
                        Err(e) => tracing::error!(error = %e, "has_metric_row failed"),
                    }
                }
            }
        }
    });
}

// ---- read APIs (JSON shapes match growth-chart) ----------------------------

pub async fn query_downloads(
    db: &Db,
    repo: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<Value, sqlx::Error> {
    let mut sql = String::from(
        "SELECT repo, date, tag, download_count FROM github_downloads WHERE 1=1",
    );
    if repo.is_some() {
        sql.push_str(" AND repo = ?");
    }
    if from.is_some() {
        sql.push_str(" AND date >= ?");
    }
    if to.is_some() {
        sql.push_str(" AND date <= ?");
    }
    sql.push_str(" ORDER BY date DESC, tag ASC");
    let mut q = sqlx::query(&sql);
    if let Some(r) = repo {
        q = q.bind(r);
    }
    if let Some(f) = from {
        q = q.bind(f);
    }
    if let Some(t) = to {
        q = q.bind(t);
    }
    let rows = q.fetch_all(db.pool()).await?;
    let out: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "repo": r.get::<String, _>("repo"),
                "date": r.get::<String, _>("date"),
                "tag": r.get::<String, _>("tag"),
                "download_count": r.get::<i64, _>("download_count"),
            })
        })
        .collect();
    Ok(Value::Array(out))
}

/// Day-over-day download deltas (ported from growth-chart `dailyDownloadDeltas`).
pub fn daily_download_deltas(rows: &[Value]) -> Value {
    use std::collections::BTreeMap;
    let mut by_date: BTreeMap<String, Vec<(String, i64)>> = BTreeMap::new();
    for r in rows {
        let date = r.get("date").and_then(Value::as_str).unwrap_or("").to_string();
        let tag = r.get("tag").and_then(Value::as_str).unwrap_or("").to_string();
        let count = r.get("download_count").and_then(Value::as_i64).unwrap_or(0);
        by_date.entry(date).or_default().push((tag, count));
    }
    let dates: Vec<String> = by_date.keys().cloned().collect();
    let mut deltas = Vec::new();
    for i in 1..dates.len() {
        let prev: std::collections::HashMap<_, _> = by_date[&dates[i - 1]]
            .iter()
            .cloned()
            .collect();
        let mut total = 0i64;
        let mut releases = Vec::new();
        for (tag, count) in &by_date[&dates[i]] {
            let delta = count - prev.get(tag).copied().unwrap_or(0);
            if delta != 0 {
                releases.push(json!({ "tag": tag, "delta": delta }));
                total += delta;
            }
        }
        deltas.push(json!({
            "date": dates[i],
            "total": total,
            "releases": releases,
        }));
    }
    Value::Array(deltas)
}

pub async fn downloads_daily(db: &Db, repo: Option<&str>) -> Result<Value, sqlx::Error> {
    let mut sql =
        String::from("SELECT repo, date, tag, download_count FROM github_downloads WHERE 1=1");
    if repo.is_some() {
        sql.push_str(" AND repo = ?");
    }
    sql.push_str(" ORDER BY date ASC, tag ASC");
    let mut q = sqlx::query(&sql);
    if let Some(r) = repo {
        q = q.bind(r);
    }
    let rows = q.fetch_all(db.pool()).await?;
    let vals: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "repo": r.get::<String, _>("repo"),
                "date": r.get::<String, _>("date"),
                "tag": r.get::<String, _>("tag"),
                "download_count": r.get::<i64, _>("download_count"),
            })
        })
        .collect();
    Ok(daily_download_deltas(&vals))
}

pub async fn downloads_latest(db: &Db, repo: Option<&str>) -> Result<Value, sqlx::Error> {
    let mut date_sql = String::from("SELECT date FROM github_downloads WHERE 1=1");
    if repo.is_some() {
        date_sql.push_str(" AND repo = ?");
    }
    date_sql.push_str(" ORDER BY date DESC LIMIT 1");
    let mut q = sqlx::query(&date_sql);
    if let Some(r) = repo {
        q = q.bind(r);
    }
    let latest = q.fetch_optional(db.pool()).await?;
    let Some(latest) = latest else {
        return Ok(json!({ "date": null, "total": 0, "releases": [] }));
    };
    let date: String = latest.get("date");
    let mut rows_sql =
        String::from("SELECT tag, download_count FROM github_downloads WHERE date = ?");
    if repo.is_some() {
        rows_sql.push_str(" AND repo = ?");
    }
    rows_sql.push_str(" ORDER BY tag ASC");
    let mut q = sqlx::query(&rows_sql).bind(&date);
    if let Some(r) = repo {
        q = q.bind(r);
    }
    let rows = q.fetch_all(db.pool()).await?;
    let mut total = 0i64;
    let releases: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            let c: i64 = r.get("download_count");
            total += c;
            json!({ "tag": r.get::<String, _>("tag"), "download_count": c })
        })
        .collect();
    Ok(json!({ "date": date, "total": total, "releases": releases }))
}

pub async fn backfill_downloads(
    db: &Db,
    repo: &str,
    date: &str,
    total: i64,
) -> Result<Value, String> {
    let nearest: Option<String> = sqlx::query_scalar(
        "SELECT date FROM github_downloads WHERE repo = ? ORDER BY ABS(julianday(date) - julianday(?)) LIMIT 1",
    )
    .bind(repo)
    .bind(date)
    .fetch_optional(db.pool())
    .await
    .map_err(|e| e.to_string())?;
    let Some(nearest) = nearest else {
        return Err("No existing snapshots to base distribution on".into());
    };
    let ref_rows = sqlx::query(
        "SELECT tag, download_count FROM github_downloads WHERE repo = ? AND date = ?",
    )
    .bind(repo)
    .bind(&nearest)
    .fetch_all(db.pool())
    .await
    .map_err(|e| e.to_string())?;
    let ref_total: i64 = ref_rows
        .iter()
        .map(|r| r.get::<i64, _>("download_count"))
        .sum();
    if ref_total <= 0 {
        return Err("Reference snapshot has zero downloads".into());
    }
    let mut results = Vec::new();
    let mut assigned = 0i64;
    for (i, r) in ref_rows.iter().enumerate() {
        let tag: String = r.get("tag");
        let count = r.get::<i64, _>("download_count");
        let is_last = i + 1 == ref_rows.len();
        let n = if is_last {
            (total - assigned).max(0)
        } else {
            ((count as f64 / ref_total as f64) * total as f64).round() as i64
        }
        .max(0);
        assigned += n;
        sqlx::query(
            "INSERT OR REPLACE INTO github_downloads (repo, date, tag, download_count) VALUES (?, ?, ?, ?)",
        )
        .bind(repo)
        .bind(date)
        .bind(&tag)
        .bind(n)
        .execute(db.pool())
        .await
        .map_err(|e| e.to_string())?;
        results.push(json!({ "tag": tag, "download_count": n }));
    }
    Ok(json!({
        "repo": repo,
        "date": date,
        "total": total,
        "releases": results,
    }))
}

pub async fn query_metrics(
    db: &Db,
    metric: &str,
    repo: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<Value, sqlx::Error> {
    let mut sql = String::from(
        "SELECT repo, date, metric, count, uniques FROM github_metrics WHERE metric = ?",
    );
    if repo.is_some() {
        sql.push_str(" AND repo = ?");
    }
    if from.is_some() {
        sql.push_str(" AND date >= ?");
    }
    if to.is_some() {
        sql.push_str(" AND date <= ?");
    }
    sql.push_str(" ORDER BY date DESC");
    let mut q = sqlx::query(&sql).bind(metric);
    if let Some(r) = repo {
        q = q.bind(r);
    }
    if let Some(f) = from {
        q = q.bind(f);
    }
    if let Some(t) = to {
        q = q.bind(t);
    }
    let rows = q.fetch_all(db.pool()).await?;
    let out: Vec<Value> = rows
        .into_iter()
        .map(|r| {
            json!({
                "repo": r.get::<String, _>("repo"),
                "date": r.get::<String, _>("date"),
                "metric": r.get::<String, _>("metric"),
                "count": r.get::<i64, _>("count"),
                "uniques": r.get::<i64, _>("uniques"),
            })
        })
        .collect();
    Ok(Value::Array(out))
}

pub async fn metrics_daily(
    db: &Db,
    metric: &str,
    repo: Option<&str>,
) -> Result<Value, sqlx::Error> {
    let mut sql = String::from(
        "SELECT date, SUM(count) as count FROM github_metrics WHERE metric = ?",
    );
    if repo.is_some() {
        sql.push_str(" AND repo = ?");
    }
    sql.push_str(" GROUP BY date ORDER BY date ASC");
    let mut q = sqlx::query(&sql).bind(metric);
    if let Some(r) = repo {
        q = q.bind(r);
    }
    let rows = q.fetch_all(db.pool()).await?;
    let counts: Vec<(String, i64)> = rows
        .into_iter()
        .map(|r| (r.get::<String, _>("date"), r.get::<i64, _>("count")))
        .collect();
    let mut deltas = Vec::new();
    for i in 1..counts.len() {
        deltas.push(json!({
            "date": counts[i].0,
            "total": counts[i].1 - counts[i - 1].1,
        }));
    }
    Ok(Value::Array(deltas))
}

pub async fn metrics_latest(
    db: &Db,
    metric: &str,
    repo: Option<&str>,
) -> Result<Value, sqlx::Error> {
    let mut sql = String::from(
        "SELECT date, SUM(count) as count, SUM(uniques) as uniques FROM github_metrics WHERE metric = ?",
    );
    if repo.is_some() {
        sql.push_str(" AND repo = ?");
    }
    sql.push_str(" GROUP BY date ORDER BY date DESC LIMIT 1");
    let mut q = sqlx::query(&sql).bind(metric);
    if let Some(r) = repo {
        q = q.bind(r);
    }
    let row = q.fetch_optional(db.pool()).await?;
    let Some(row) = row else {
        return Ok(json!({ "date": null, "count": 0, "uniques": 0 }));
    };
    let date: Option<String> = row.try_get("date").ok();
    if date.is_none() {
        return Ok(json!({ "date": null, "count": 0, "uniques": 0 }));
    }
    Ok(json!({
        "date": date,
        "count": row.get::<i64, _>("count"),
        "uniques": row.get::<i64, _>("uniques"),
    }))
}

/// Every metric a growth rate can be asked for (`downloads` lives in its own table).
pub const VALID_RATE_METRICS: &[&str] = &["downloads", "stars", "forks", "clones", "views"];

/// Rolling week-over-week growth rate (%) per snapshot date, oldest first.
///
/// Each point compares the metric's level on `date` against the latest snapshot
/// at-or-before `date - 7 days` — the same rule the dashboard chart draws, so the
/// API and the UI can never disagree. Dates with no 7-day-back baseline (the first
/// week of history) or a non-positive one are omitted, so an 8-day-old repo yields
/// one point. `clones`/`views` are per-day traffic counts, not levels, so they are
/// accumulated first (a rate needs a level; a rate of a delta is noise).
///
/// One query: SQLite `date(a.date,'-7 day')` does the calendar math, so no date
/// crate is pulled in, and the correlated subquery does the at-or-before lookup.
pub async fn wow_points(
    db: &Db,
    metric: &str,
    repo: Option<&str>,
) -> Result<Vec<Value>, sqlx::Error> {
    let downloads = metric == "downloads";
    let cumulative = metric == "clones" || metric == "views";
    let mut base = if downloads {
        String::from("SELECT date, SUM(download_count) AS v FROM github_downloads WHERE 1=1")
    } else {
        String::from("SELECT date, SUM(count) AS v FROM github_metrics WHERE metric = ?")
    };
    if repo.is_some() {
        base.push_str(" AND repo = ?");
    }
    base.push_str(" GROUP BY date");
    // Traffic metrics: running total over the recorded days, then rate that level.
    let series = if cumulative {
        format!("WITH b AS ({base}), s AS (SELECT date, SUM(v) OVER (ORDER BY date) AS v FROM b)")
    } else {
        format!("WITH s AS ({base})")
    };
    let sql = format!(
        "{series} SELECT a.date AS date, a.v AS v, \
         (SELECT c.date FROM s c WHERE c.date <= date(a.date,'-7 day') ORDER BY c.date DESC LIMIT 1) AS prev_date, \
         (SELECT c.v FROM s c WHERE c.date <= date(a.date,'-7 day') ORDER BY c.date DESC LIMIT 1) AS prev_v \
         FROM s a ORDER BY a.date ASC"
    );
    let mut q = sqlx::query(&sql);
    if !downloads {
        q = q.bind(metric);
    }
    if let Some(r) = repo {
        q = q.bind(r);
    }
    let rows = q.fetch_all(db.pool()).await?;
    let mut out = Vec::new();
    for r in rows {
        let (Ok(prev_date), Ok(prev_v)) = (
            r.try_get::<String, _>("prev_date"),
            r.try_get::<i64, _>("prev_v"),
        ) else {
            continue; // inside the first week — no baseline to compare against
        };
        if prev_v <= 0 {
            continue;
        }
        let date: String = r.get("date");
        if prev_date == date {
            continue; // only one snapshot that week
        }
        let v: i64 = r.get("v");
        let rate = ((v - prev_v) as f64 / prev_v as f64) * 100.0;
        out.push(json!({
            "date": date,
            "value": v,
            "prev_date": prev_date,
            "prev_value": prev_v,
            "rate": (rate * 100.0).round() / 100.0,
        }));
    }
    Ok(out)
}

/// `GET /api/{metrics,downloads}/wow` body: the full rate series + its last point.
pub async fn wow_series(
    db: &Db,
    metric: &str,
    repo: Option<&str>,
) -> Result<Value, sqlx::Error> {
    let points = wow_points(db, metric, repo).await?;
    Ok(json!({
        "repo": repo,
        "metric": metric,
        "latest": points.last().cloned().unwrap_or(Value::Null),
        "points": points,
    }))
}

/// Per-repo WoW for `metric` — the answer when no `repo` is named. Summing
/// download counts across projects produces a blended rate that describes no
/// actual project: one repo's release week drags the number for everything
/// else. Returns each repo's latest point (no series; ask for a repo to get
/// that), most-recently-dated and fastest-growing first.
pub async fn wow_series_by_repo(db: &Db, metric: &str) -> Result<Value, sqlx::Error> {
    let repos: Vec<String> = sqlx::query_scalar("SELECT repo FROM github_repos ORDER BY repo ASC")
        .fetch_all(db.pool())
        .await?;
    let mut rows = Vec::new();
    for repo in &repos {
        let points = wow_points(db, metric, Some(repo)).await?;
        let Some(latest) = points.last() else { continue };
        let mut row = latest.clone();
        if let Some(obj) = row.as_object_mut() {
            obj.insert("repo".into(), json!(repo));
        }
        rows.push(row);
    }
    rows.sort_by(|a, b| {
        let r = |v: &Value| v.get("rate").and_then(Value::as_f64).unwrap_or(f64::MIN);
        r(b).partial_cmp(&r(a)).unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(json!({ "metric": metric, "repos": rows }))
}

/// Latest WoW rate per metric (`null` until a repo has 8+ days of history).
async fn wow_latest_rates(db: &Db, repo: &str) -> Result<Value, sqlx::Error> {
    let mut map = serde_json::Map::new();
    for metric in VALID_RATE_METRICS {
        let last = wow_points(db, metric, Some(repo)).await?;
        let rate = last
            .last()
            .and_then(|p| p.get("rate").cloned())
            .unwrap_or(Value::Null);
        map.insert((*metric).to_string(), rate);
    }
    Ok(Value::Object(map))
}

/// Repo whose `linked_site_id` is this site uuid (or `"native"` for Dottie desktop).
pub async fn repo_for_link(db: &Db, link: &str) -> Result<Option<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT repo FROM github_repos WHERE linked_site_id = ? LIMIT 1")
        .bind(link)
        .fetch_optional(db.pool())
        .await
}

/// Compact latest growth block for embedding on site / daily reports.
pub async fn growth_brief(db: &Db, repo: &str) -> Result<Value, sqlx::Error> {
    let dl = downloads_latest(db, Some(repo)).await?;
    let stars = metrics_latest(db, "stars", Some(repo)).await?;
    let forks = metrics_latest(db, "forks", Some(repo)).await?;
    let clones = metrics_latest(db, "clones", Some(repo)).await?;
    let views = metrics_latest(db, "views", Some(repo)).await?;
    Ok(json!({
        "repo": repo,
        "downloads": dl.get("total").cloned().unwrap_or(json!(0)),
        "downloads_date": dl.get("date").cloned().unwrap_or(Value::Null),
        "stars": stars.get("count").cloned().unwrap_or(json!(0)),
        "forks": forks.get("count").cloned().unwrap_or(json!(0)),
        "clones": clones.get("count").cloned().unwrap_or(json!(0)),
        "unique_clones": clones.get("uniques").cloned().unwrap_or(json!(0)),
        "views": views.get("count").cloned().unwrap_or(json!(0)),
        "unique_views": views.get("uniques").cloned().unwrap_or(json!(0)),
        "metrics_date": stars.get("date").cloned().unwrap_or(Value::Null),
        // Latest week-over-week % per metric, so every caller that already reads a
        // brief (sites list, site summary, daily/site-daily report) gets the growth
        // rate without a second request or a changed request shape.
        "wow": wow_latest_rates(db, repo).await?,
    }))
}

/// Attach `repo` growth brief when a link exists; leaves body unchanged otherwise.
pub async fn attach_linked_growth(
    db: &Db,
    link: &str,
    mut body: Value,
) -> Result<Value, sqlx::Error> {
    if let Some(repo) = repo_for_link(db, link).await? {
        body.as_object_mut()
            .map(|o| o.insert("repo".into(), json!(repo.clone())));
        let brief = growth_brief(db, &repo).await?;
        if let Some(o) = body.as_object_mut() {
            o.insert("growth".into(), brief);
        }
    }
    Ok(body)
}

/// Ensure `owner/name` is tracked and set `linked_site_id` to this site id.
/// Adds the repo (validates on GitHub) if missing; otherwise only updates the link.
/// Always kicks an immediate full snapshot; hourly collector covers it thereafter.
pub async fn ensure_repo_linked_to_site(
    db: &Db,
    cfg: &GithubConfig,
    repo: &str,
    site_id: &str,
) -> Result<Value, String> {
    let repo = repo.trim();
    if repo.is_empty() || !repo.contains('/') {
        return Err("Repo must be in \"owner/name\" format".into());
    }
    // Prefer existing row; otherwise add (GitHub-validated + initial snapshot).
    let existing = sqlx::query("SELECT id FROM github_repos WHERE repo = ?")
        .bind(repo)
        .fetch_optional(db.pool())
        .await
        .map_err(|e| e.to_string())?;
    let (id, already_tracked) = if let Some(row) = existing {
        (row.get::<i64, _>("id"), true)
    } else {
        let added = add_repo(db, cfg, repo, Some("repo")).await?;
        let id = added
            .get("id")
            .and_then(Value::as_i64)
            .ok_or_else(|| "add_repo missing id".to_string())?;
        (id, false)
    };
    // Force link to this site (overrides prior link / GITHUB_SITE_LINKS seed).
    let out = match patch_repo(db, id, None, Some(Some(site_id))).await {
        Ok(Some(v)) => v,
        Ok(None) => return Err("repo not found after ensure".into()),
        Err(e) => return Err(e),
    };
    // add_repo already snapshots new rows; re-snapshot existing so KPIs fill in.
    if already_tracked {
        spawn_repo_snapshot(db.clone(), cfg.clone(), repo.to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_site_links_trims_and_skips_malformed() {
        let links = parse_site_links(" o/a = site-1 ,o/b=native,,bad=x,o/c=, noequals");
        assert_eq!(
            links,
            vec![
                ("o/a".to_string(), "site-1".to_string()),
                ("o/b".to_string(), "native".to_string()),
            ]
        );
        assert!(parse_site_links("").is_empty());
    }

    // Empty config seeds nothing; configured links fill only unset rows.
    #[sqlx::test]
    async fn seed_repos_uses_config_only(pool: sqlx::SqlitePool) {
        let db = Db::from_pool(pool, Default::default(), Default::default());
        seed_repos(&db, &GithubConfig::default()).await.unwrap();
        assert!(list_repo_names(&db).await.unwrap().is_empty());

        let cfg = GithubConfig {
            token: String::new(),
            seed_repos: vec![("o/a".into(), "repo"), ("o/b".into(), "repo")],
            site_links: vec![("o/a".into(), "native".into()), ("o/b".into(), "s1".into())],
        };
        sqlx::query("INSERT INTO github_repos (repo, kind, linked_site_id) VALUES ('o/b', 'repo', 'kept')")
            .execute(db.pool())
            .await
            .unwrap();
        seed_repos(&db, &cfg).await.unwrap();
        // Table was not empty, so o/a is not seeded; o/b keeps its link.
        assert_eq!(list_repo_names(&db).await.unwrap(), vec!["o/b".to_string()]);
        let link: String =
            sqlx::query_scalar("SELECT linked_site_id FROM github_repos WHERE repo = 'o/b'")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(link, "kept");

        // GitHub names are case-insensitive: a mixed-case config still links.
        sqlx::query("INSERT INTO github_repos (repo, kind) VALUES ('O/Mixed', 'repo')")
            .execute(db.pool())
            .await
            .unwrap();
        let cfg = GithubConfig {
            site_links: vec![("o/mixed".into(), "s2".into())],
            ..cfg
        };
        assert_eq!(cfg.link_for("O/MIXED"), Some("s2"));
        seed_repos(&db, &cfg).await.unwrap();
        let link: String =
            sqlx::query_scalar("SELECT linked_site_id FROM github_repos WHERE repo = 'O/Mixed'")
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(link, "s2");
    }

    // 404 auto-disable: the collectors stop listing a repo GitHub 404s, its
    // history stays, and a PATCH re-enables it. Non-404 errors don't disable.
    #[sqlx::test]
    async fn disable_on_404_skips_and_patch_reenables(pool: sqlx::SqlitePool) {
        let db = Db::from_pool(pool, Default::default(), Default::default());
        sqlx::query("INSERT INTO github_repos (repo, kind) VALUES ('o/gone', 'repo'), ('o/fine', 'repo')")
            .execute(db.pool())
            .await
            .unwrap();

        assert!(!disable_repo_on_404(&db, "o/fine", "GitHub API returned 500 Internal Server Error").await);
        assert!(disable_repo_on_404(&db, "o/gone", "GitHub API returned 404 Not Found").await);

        let names = list_repo_names(&db).await.unwrap();
        assert_eq!(names, vec!["o/fine".to_string()]);

        let id: i64 = sqlx::query_scalar("SELECT id FROM github_repos WHERE repo = 'o/gone'")
            .fetch_one(db.pool())
            .await
            .unwrap();
        let row = patch_repo(&db, id, Some("repo"), None).await.unwrap().unwrap();
        assert!(row["disabled_at"].is_null());
        assert_eq!(list_repo_names(&db).await.unwrap().len(), 2);
    }

    #[test]
    fn civil_date_epoch() {
        assert_eq!(civil_date(0), "1970-01-01");
        // 2026-07-12 ≈ known
        assert_eq!(civil_date(20_646), "2026-07-12");
    }

    #[test]
    fn daily_deltas_basic() {
        let rows = vec![
            json!({"date": "2026-01-01", "tag": "v1", "download_count": 10}),
            json!({"date": "2026-01-02", "tag": "v1", "download_count": 15}),
        ];
        let d = daily_download_deltas(&rows);
        let arr = d.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["total"], 5);
    }
}
