//! The telemetry data layer — a faithful sqlx async port of `backend/db.js`.
//! Owns pool/pragma setup, the final-form schema (applied via the
//! `migrations/` directory), the dev/internal ingest-exclusion logic, per-user
//! bearer tokens, and the write paths (register / ping / event / diagnostic /
//! landing / rejected). The ~9 report/read functions and the boot-time scrub
//! are a later phase (see TODOs at the bottom).
//!
//! QUERY API: we use the sqlx RUNTIME query API (`sqlx::query(..).bind(..)`,
//! `query_as`, `fetch_optional`/`execute`) rather than the compile-time
//! `query!` macros. This keeps the build hermetic — no DATABASE_URL needed at
//! compile time and no `.sqlx` offline cache to maintain.
//!
//! EXCLUSION CONFIG: db.js freezes the excluded id/email sets from env at module
//! load, which is impossible to exercise from a test. Here the sets are injected
//! into `Db` at construction. `main.rs` reads the env vars and passes them in;
//! tests inject directly.

#![allow(dead_code)]

use std::collections::HashSet;
use std::io::Read;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions};
use sqlx::{FromRow, Row, SqlitePool};

use crate::reports::ReportExclude;

/// The row returned by `get_user_by_token`. Includes Stripe subscription fields
/// for Pro paywall (`admit` skips free credit when status is active/trialing).
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct UserRow {
    pub id: i64,
    pub install_id: String,
    pub email: String,
    pub subscription_stripe_id: Option<String>,
    pub subscription_status: Option<String>,
    pub subscription_expires: Option<i64>,
}

impl UserRow {
    /// Paid Pro access (Stripe). Free credit may still be exhausted; this is
    /// independent entitlement. A past `subscription_expires` (unix seconds)
    /// treats a missed cancel webhook as unpaid.
    pub fn is_paid(&self) -> bool {
        if !matches!(
            self.subscription_status.as_deref(),
            Some("active") | Some("trialing")
        ) {
            return false;
        }
        match self.subscription_expires {
            Some(exp) if exp > 0 => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0);
                exp > now
            }
            _ => true,
        }
    }
}

/// One row for the `pro_calls` audit ledger. Borrowed strings: every field
/// comes from data the handler already holds, so nothing is cloned onto the
/// hot path. Content is never included — route/model/status/token counts only.
pub struct ProCall<'a> {
    pub user_id: i64,
    pub email: &'a str,
    /// `chat` | `stt` | `tts` | `voice_mint`
    pub route: &'a str,
    pub model: Option<&'a str>,
    /// `ok` | `rejected` | `upstream_error`
    pub outcome: &'a str,
    /// Which cap refused it, or how the upstream call failed.
    pub reason: Option<&'a str>,
    pub status: Option<i64>,
    pub prompt_tokens: Option<i64>,
    pub completion_tokens: Option<i64>,
    /// What was actually billed to the user's counters.
    pub tokens: i64,
    /// True when `tokens` is a synthetic estimate (audio, voice) rather than a
    /// token count upstream reported.
    pub estimated: bool,
    /// STT audio seconds, or the voice TTL granted.
    pub seconds: Option<f64>,
    pub paid: bool,
    pub ip: Option<&'a str>,
    /// Real spend in xAI ticks (1e-10 USD). From `usage.cost_in_usd_ticks` on
    /// chat; an estimate on the audio routes, where xAI reports no cost.
    pub cost_ticks: i64,
    /// True when `cost_ticks` is our estimate rather than an upstream figure.
    pub cost_estimated: bool,
}

fn pro_call_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    json!({
        "id": r.get::<i64, _>("id"),
        "user_id": r.get::<i64, _>("user_id"),
        "email": r.get::<Option<String>, _>("email"),
        "route": r.get::<String, _>("route"),
        "model": r.get::<Option<String>, _>("model"),
        "outcome": r.get::<String, _>("outcome"),
        "reason": r.get::<Option<String>, _>("reason"),
        "status": r.get::<Option<i64>, _>("status"),
        "prompt_tokens": r.get::<Option<i64>, _>("prompt_tokens"),
        "completion_tokens": r.get::<Option<i64>, _>("completion_tokens"),
        "tokens": r.get::<i64, _>("tokens"),
        "estimated": r.get::<i64, _>("estimated") == 1,
        "seconds": r.get::<Option<f64>, _>("seconds"),
        "paid": r.get::<i64, _>("paid") == 1,
        "ip": r.get::<Option<String>, _>("ip"),
        "cost_ticks": r.get::<i64, _>("cost_ticks"),
        "cost_usd": ticks_to_usd(r.get::<i64, _>("cost_ticks")),
        "cost_estimated": r.get::<i64, _>("cost_estimated") == 1,
        "at": r.get::<i64, _>("created_at"),
    })
}

/// xAI bills in "ticks": 1 tick = 1e-10 USD, so 10^10 ticks = $1. Verified
/// against the published price table — a call with 2 uncached + 640 cached
/// input and 115 output tokens on grok-4.6 reported 10_140_000 ticks, which is
/// exactly `2*$2 + 640*$0.50 + 115*$6` per million = $0.001014.
pub const TICKS_PER_USD: i64 = 10_000_000_000;

/// Ticks as dollars. Rounded to 8 decimals — enough to keep a single sub-cent
/// call visible (a short chat turn costs ~$0.001) while trimming float noise.
/// Rounding any coarser silently zeroes real spend.
pub fn ticks_to_usd(ticks: i64) -> f64 {
    (ticks as f64 / TICKS_PER_USD as f64 * 1e8).round() / 1e8
}

/// Inputs to `insert_refund_request` — the usage snapshot is what the operator
/// reviews before approving (heavy use then instant refund = abuse signal).
pub struct RefundRequestInput<'a> {
    pub user_id: i64,
    pub email: &'a str,
    pub customer: &'a str,
    pub subscription_id: &'a str,
    pub requests_today: i64,
    pub tokens_today: i64,
    pub total_requests: i64,
    pub total_tokens: i64,
}

fn refund_request_json(r: &sqlx::sqlite::SqliteRow) -> Value {
    serde_json::json!({
        "id": r.get::<i64, _>("id"),
        "user_id": r.get::<i64, _>("user_id"),
        "email": r.get::<String, _>("email"),
        "customer": r.get::<String, _>("customer"),
        "subscription_id": r.get::<String, _>("subscription_id"),
        "status": r.get::<String, _>("status"),
        "requests_today": r.get::<i64, _>("requests_today"),
        "tokens_today": r.get::<i64, _>("tokens_today"),
        "total_requests": r.get::<i64, _>("total_requests"),
        "total_tokens": r.get::<i64, _>("total_tokens"),
        "note": r.get::<Option<String>, _>("note"),
        "created_at": r.get::<i64, _>("created_at"),
        "resolved_at": r.get::<Option<i64>, _>("resolved_at"),
    })
}

/// Inputs to `register_user`. Optional fields mirror JS `?? null` semantics.
/// `iid` is the landing-page install id (Phase 1 attribution): when the desktop
/// client forwards the `iid` it captured from the marketing site, register maps
/// install_id → iid so signups can be attributed to a landing_visits UTM row.
#[derive(Debug, Clone, Default)]
pub struct RegisterUser {
    pub name: String,
    pub email: String,
    pub install_id: String,
    pub version: Option<String>,
    pub os_version: Option<String>,
    pub country: Option<String>,
    pub iid: Option<String>,
    pub device_id: Option<String>,
    /// Client IP at registration. Stored so the per-IP registration throttle
    /// survives a restart (the in-memory rate limiter does not).
    pub ip: Option<String>,
}

/// Inputs to `record_ping`.
#[derive(Debug, Clone, Default)]
pub struct Ping {
    pub install_id: String,
    pub version: Option<String>,
}

/// Inputs to `record_event`. `props` is arbitrary JSON serialized to a TEXT
/// column; `event_id` (when non-empty) enables the idempotent OR IGNORE path.
#[derive(Debug, Clone, Default)]
pub struct Event {
    pub install_id: String,
    pub name: String,
    pub url: Option<String>,
    pub props: Option<Value>,
    pub version: Option<String>,
    pub event_id: Option<String>,
}

/// Inputs to `record_diagnostic`. The three JSON columns serialize to TEXT.
#[derive(Debug, Clone, Default)]
pub struct Diagnostic {
    pub install_id: String,
    pub version: Option<String>,
    pub fingerprint: Option<Value>,
    pub logs: Option<Value>,
    pub gateway: Option<Value>,
    pub note: Option<String>,
}

/// Inputs to `record_landing_visit`. UTM fields use COALESCE-preserves-first on
/// conflict (see the upsert SQL).
#[derive(Debug, Clone, Default)]
pub struct LandingVisit {
    pub iid: String,
    pub ip: Option<String>,
    pub ua: Option<String>,
    pub path: Option<String>,
    pub referrer: Option<String>,
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
    pub utm_term: Option<String>,
    pub utm_content: Option<String>,
}

/// A named landing-page event (the Umami mirror). `props` is an arbitrary JSON
/// object (serialized to the TEXT column); `iid` ties it to the `landing_visits`
/// row and, via `install_landing_map`, to the eventual signup.
#[derive(Debug, Clone, Default)]
pub struct LandingEvent {
    pub iid: String,
    pub name: String,
    pub props: Option<Value>,
    pub path: Option<String>,
    pub ip: Option<String>,
    pub ua: Option<String>,
}

/// One generic multi-tenant web-analytics event (the Umami-shaped store). Not
/// tied to a Dottie install — belongs to a `site` and a visitor `distinct_id`.
/// `event_type` mirrors Umami: 1 = pageview, 2 = custom event. `created_at` is
/// explicit (epoch-ms) so historical imports preserve the original time.
#[derive(Debug, Clone, Default)]
pub struct SiteEvent {
    pub event_type: i64,
    pub name: Option<String>,
    pub url: Option<String>,
    pub referrer: Option<String>,
    pub distinct_id: Option<String>,
    pub session_id: Option<String>,
    pub hostname: Option<String>,
    pub browser: Option<String>,
    pub os: Option<String>,
    pub device: Option<String>,
    pub country: Option<String>,
    pub utm_source: Option<String>,
    pub utm_medium: Option<String>,
    pub utm_campaign: Option<String>,
    pub props: Option<Value>,
    pub created_at: i64,
    /// Raw User-Agent, clamped to 300 chars. Set by `/api/collect`; `None` for
    /// bulk imports, which have no UA to carry.
    pub ua: Option<String>,
    /// 1 = bot/scraper (stored for audit); reports filter `bot = 0`.
    pub bot: i64,
    /// Umami `event_id` for an imported row. `None` on live collect. When set,
    /// a repeat import of the same id is a no-op.
    pub source_event_id: Option<String>,
}

/// Inputs to `record_rejected_registration`. Per-field length clamps match
/// db.js exactly (see the function).
#[derive(Debug, Clone, Default)]
pub struct RejectedRegistration {
    pub name: Option<String>,
    pub email: Option<String>,
    pub install_id: Option<String>,
    pub version: Option<String>,
    pub os_version: Option<String>,
    pub country: Option<String>,
    pub reason: String,
    pub ip: Option<String>,
    pub ua: Option<String>,
}

/// The data layer. An sqlx `SqlitePool` plus the injected exclusion config.
#[derive(Clone)]
pub struct Db {
    /// HS256 secret the Pro relay verifies with (`PRO_JWT_SECRET`). Empty keeps
    /// the legacy random-token behavior.
    pub jwt_secret: String,
    pool: SqlitePool,
    excluded_install_ids: HashSet<String>,
    excluded_emails: HashSet<String>,
    /// Daily-report person-list filter. Empty by default; `main.rs` injects it
    /// from env via `with_report_exclude`.
    report_exclude: ReportExclude,
}

impl Db {
    /// Open (or create) the DB at `path`, configure pragmas, run migrations,
    /// and capture the exclusion config. `excluded_emails` must already be
    /// lowercased by the caller (`main.rs` does this when reading env); matching
    /// here lowercases the candidate email before lookup.
    pub async fn connect(
        path: &str,
        excluded_install_ids: HashSet<String>,
        excluded_emails: HashSet<String>,
    ) -> Result<Db, sqlx::Error> {
        // Pragmas mirror db.js: WAL, foreign keys on, ~2MB page cache, temp
        // tables in memory. cache_size/temp_store are best-effort via .pragma().
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{path}"))?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .foreign_keys(true)
            .pragma("cache_size", "-2000")
            .pragma("mmap_size", "0")
            .pragma("temp_store", "MEMORY");

        let pool = SqlitePoolOptions::new().connect_with(opts).await?;
        let db = Db {
            pool,
            excluded_install_ids,
            excluded_emails,
            report_exclude: ReportExclude::default(),
            // Read here rather than threaded through every caller: the secret
            // is deploy config, and every construction path wants the same one.
            jwt_secret: std::env::var("PRO_JWT_SECRET").unwrap_or_default(),
        };
        db.migrate().await?;
        Ok(db)
    }

    /// Build a `Db` over an already-constructed pool (used by `#[sqlx::test]`,
    /// which provisions its own pool and runs the migrations for us).
    pub fn from_pool(
        pool: SqlitePool,
        excluded_install_ids: HashSet<String>,
        excluded_emails: HashSet<String>,
    ) -> Db {
        Db {
            pool,
            excluded_install_ids,
            excluded_emails,
            report_exclude: ReportExclude::default(),
            jwt_secret: std::env::var("PRO_JWT_SECRET").unwrap_or_default(),
        }
    }

    /// Set the daily-report person-list filter (see `ReportExclude`).
    pub fn with_report_exclude(mut self, exclude: ReportExclude) -> Db {
        self.report_exclude = exclude;
        self
    }

    pub fn report_exclude(&self) -> &ReportExclude {
        &self.report_exclude
    }

    /// Run the `migrations/` directory, then the idempotent additive schema
    /// step. Safe on a fresh DB, an already-migrated DB, AND an under-migrated
    /// one (an older DB whose events table predates `event_id`, etc.).
    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        // migrations/ through 0020. 0020 adds the feedback table.
        sqlx::migrate!().run(&self.pool).await?;
        self.ensure_schema().await?;
        Ok(())
    }

    /// Faithful port of db.js's additive migrations: add the columns that older
    /// DBs predate (gated on `PRAGMA table_info` since SQLite has no
    /// `ADD COLUMN IF NOT EXISTS`), backfill tokens, then create the partial
    /// unique indexes that depend on those columns. Idempotent: on a fresh DB
    /// the columns already exist (from 0001) so only the indexes get created; on
    /// an under-migrated DB the missing columns are added first.
    async fn ensure_schema(&self) -> Result<(), sqlx::Error> {
        let users = self.column_names("users").await?;
        if !users.contains("country") {
            sqlx::query("ALTER TABLE users ADD COLUMN country TEXT")
                .execute(&self.pool)
                .await?;
        }
        if !users.contains("token") {
            sqlx::query("ALTER TABLE users ADD COLUMN token TEXT")
                .execute(&self.pool)
                .await?;
            // Backfill existing rows so the partial unique index has values —
            // matches db.js's randomblob(32) hex backfill.
            sqlx::query("UPDATE users SET token = lower(hex(randomblob(32))) WHERE token IS NULL")
                .execute(&self.pool)
                .await?;
        }

        let events = self.column_names("events").await?;
        if !events.contains("event_id") {
            sqlx::query("ALTER TABLE events ADD COLUMN event_id TEXT")
                .execute(&self.pool)
                .await?;
        }

        let issues = self.column_names("issues").await?;
        if !issues.contains("last_version") {
            sqlx::query("ALTER TABLE issues ADD COLUMN last_version TEXT")
                .execute(&self.pool)
                .await?;
        }
        // The build a fix shipped in. Old clients still in the field keep
        // sending an error long after it's fixed; without this the issue
        // regresses forever (see `upsert_issue`).
        if !issues.contains("resolved_in_version") {
            sqlx::query("ALTER TABLE issues ADD COLUMN resolved_in_version TEXT")
                .execute(&self.pool)
                .await?;
        }

        // Raw User-Agent on site events. `is_bot_ua` is a substring filter, so
        // whatever it misses is invisible without the UA to look at: a live site
        // showed hundreds of "Linux visitors" a day with no way to see what
        // they were sending.
        let site_events = self.column_names("site_events").await?;
        if !site_events.contains("ua") {
            sqlx::query("ALTER TABLE site_events ADD COLUMN ua TEXT")
                .execute(&self.pool)
                .await?;
        }
        // Bot flag: store bots instead of silent drop so reports can filter
        // humans and the UA audit still sees crawlers.
        if !site_events.contains("bot") {
            sqlx::query("ALTER TABLE site_events ADD COLUMN bot INTEGER NOT NULL DEFAULT 0")
                .execute(&self.pool)
                .await?;
        }
        // Umami event id for import dedupe. Live collect leaves it NULL.
        if !site_events.contains("source_event_id") {
            sqlx::query("ALTER TABLE site_events ADD COLUMN source_event_id TEXT")
                .execute(&self.pool)
                .await?;
        }
        // Idempotent hygiene (safe every boot): UA-based bot backfill + casing.
        // Umami imports have no UA — for Mac-product marketing domains, treat
        // bare Linux (import scrapers) as bot so dottie.ai numbers match reality.
        sqlx::query(
            "UPDATE site_events SET bot = 1 WHERE COALESCE(bot, 0) = 0 AND ua IS NOT NULL AND ua <> '' AND (
               lower(ua) LIKE '%bot%' OR lower(ua) LIKE '%crawl%' OR lower(ua) LIKE '%spider%'
               OR lower(ua) LIKE '%headless%' OR lower(ua) LIKE '%python-requests%'
               OR lower(ua) LIKE '%curl/%' OR lower(ua) LIKE '%scrapy%'
               OR lower(ua) LIKE '%gptbot%' OR lower(ua) LIKE '%bytespider%'
               OR lower(ua) LIKE '%ahrefs%' OR lower(ua) LIKE '%semrush%'
               OR lower(ua) LIKE '%shap-user%'
               OR (ua LIKE '%X11; Linux%' AND ua LIKE '%Chrome/%' AND ua NOT LIKE '%Android%')
             )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "UPDATE site_events SET bot = 1
             WHERE COALESCE(bot, 0) = 0
               AND (os = 'Linux' OR lower(os) = 'linux')
               AND (ua IS NULL OR ua = '')
               AND site_id IN (
                 SELECT id FROM sites
                 WHERE lower(COALESCE(domain,'')) LIKE '%dottie.ai%'
                    OR lower(COALESCE(name,'')) LIKE '%dottie landing%'
               )",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query("UPDATE site_events SET os = 'macOS' WHERE os IN ('Mac OS', 'mac os', 'MacOS')")
            .execute(&self.pool)
            .await?;
        sqlx::query("UPDATE site_events SET browser = 'Chrome' WHERE lower(browser) = 'chrome'")
            .execute(&self.pool)
            .await?;
        sqlx::query("UPDATE site_events SET browser = 'Safari' WHERE lower(browser) = 'safari'")
            .execute(&self.pool)
            .await?;
        sqlx::query("UPDATE site_events SET browser = 'Firefox' WHERE lower(browser) = 'firefox'")
            .execute(&self.pool)
            .await?;
        sqlx::query("UPDATE site_events SET os = 'Android' WHERE os IN ('Android OS', 'android')")
            .execute(&self.pool)
            .await?;

        // Pro paywall: Stripe customer + subscription state (additive; paid is
        // separate from free-credit counters in pro_usage).
        let users = self.column_names("users").await?;
        if !users.contains("subscription_stripe_id") {
            sqlx::query("ALTER TABLE users ADD COLUMN subscription_stripe_id TEXT")
                .execute(&self.pool)
                .await?;
        }
        if !users.contains("subscription_status") {
            sqlx::query("ALTER TABLE users ADD COLUMN subscription_status TEXT")
                .execute(&self.pool)
                .await?;
        }
        if !users.contains("subscription_expires") {
            sqlx::query("ALTER TABLE users ADD COLUMN subscription_expires INTEGER")
                .execute(&self.pool)
                .await?;
        }
        if !users.contains("ip") {
            sqlx::query("ALTER TABLE users ADD COLUMN ip TEXT")
                .execute(&self.pool)
                .await?;
        }
        // Backs `registrations_from_ip_since`: the account-minting throttle has
        // to survive a deploy, so it counts rows instead of memory buckets.
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_users_ip_created ON users(ip, created_at)")
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS stripe_webhook_events (
               event_id TEXT PRIMARY KEY,
               event_type TEXT NOT NULL,
               created_at INTEGER NOT NULL
             )",
        )
        .execute(&self.pool)
        .await?;

        // User-initiated refund requests; an operator approves or denies (the
        // approve path is what actually refunds + cancels at Stripe).
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS pro_refund_requests (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               user_id INTEGER NOT NULL,
               email TEXT NOT NULL,
               customer TEXT NOT NULL,
               subscription_id TEXT NOT NULL,
               status TEXT NOT NULL,
               requests_today INTEGER NOT NULL,
               tokens_today INTEGER NOT NULL,
               total_requests INTEGER NOT NULL,
               total_tokens INTEGER NOT NULL,
               note TEXT,
               created_at INTEGER NOT NULL,
               resolved_at INTEGER
             )",
        )
        .execute(&self.pool)
        .await?;

        // Now that token/event_id are guaranteed present, the partial unique
        // indexes are safe to create (idempotent via IF NOT EXISTS).
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_users_token ON users(token) WHERE token IS NOT NULL",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_events_event_id ON events(event_id) WHERE event_id IS NOT NULL",
        )
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_site_events_source_id ON site_events(site_id, source_event_id) WHERE source_event_id IS NOT NULL",
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Column names of `table` via `PRAGMA table_info`. `table` is always a
    /// hardcoded literal at call sites (never user input) — PRAGMA can't bind
    /// params, so it's interpolated.
    async fn column_names(&self, table: &str) -> Result<HashSet<String>, sqlx::Error> {
        let rows = sqlx::query(&format!("PRAGMA table_info({table})"))
            .fetch_all(&self.pool)
            .await?;
        let mut names = HashSet::new();
        for row in &rows {
            names.insert(row.try_get::<String, _>("name")?);
        }
        Ok(names)
    }

    /// Direct access to the underlying pool (tests, future report queries).
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    // ---- exclusion --------------------------------------------------------

    /// Release/CI smoke tests: install_id starts with `smoke-` (case-insensitive)
    /// or email ends with `@test.dottie.ai` (case-insensitive). The only standing
    /// pattern-based filter.
    pub(crate) fn is_smoke_test(install_id: Option<&str>, email: Option<&str>) -> bool {
        if let Some(iid) = install_id {
            // Byte-prefix compare: install_id is client-supplied, so `iid[..6]`
            // could panic on a non-char-boundary. "smoke-" is ASCII, so match
            // the raw bytes — no boundary hazard, same case-insensitive result.
            if iid
                .as_bytes()
                .get(..6)
                .is_some_and(|p| p.eq_ignore_ascii_case(b"smoke-"))
            {
                return true;
            }
        }
        if let Some(e) = email {
            if e.to_ascii_lowercase().ends_with("@test.dottie.ai") {
                return true;
            }
        }
        false
    }

    /// Smoke-test pattern OR membership in the injected excluded id/email sets.
    /// Email matching is case-insensitive (the set is pre-lowercased; we
    /// lowercase the candidate).
    fn is_excluded_ingest(&self, install_id: Option<&str>, email: Option<&str>) -> bool {
        if let Some(iid) = install_id {
            if self.excluded_install_ids.contains(iid) {
                return true;
            }
        }
        if let Some(e) = email {
            if self.excluded_emails.contains(&e.to_ascii_lowercase()) {
                return true;
            }
        }
        Self::is_smoke_test(install_id, email)
    }

    // ---- writes -----------------------------------------------------------

    /// How many accounts this IP has created since `since_ms`. The in-memory
    /// rate limiter resets on every deploy, which on the account-minting route
    /// hands out a fresh allowance each time; this count lives in the DB, so it
    /// does not. A re-registration from the same email updates its existing row
    /// rather than inserting, so it does not add to this count.
    pub async fn registrations_from_ip_since(
        &self,
        ip: &str,
        since_ms: i64,
    ) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM users WHERE ip = ? AND created_at >= ?",
        )
        .bind(ip)
        .bind(since_ms)
        .fetch_one(&self.pool)
        .await
    }

    /// Register (or re-register) a user. On a NEW email: INSERT, then mint a
    /// signed bearer whose claims carry the row id. On CONFLICT(email): update
    /// the other fields; a still-valid JWT is returned untouched, a legacy or
    /// expired one is replaced. That replacement is the Pro-split migration —
    /// a 404 from the relay makes the desktop re-register here. Excluded /
    /// smoke registrations return a fresh fake token but persist NO row.
    pub async fn register_user(&self, u: &RegisterUser) -> Result<String, sqlx::Error> {
        if self.is_excluded_ingest(Some(&u.install_id), Some(&u.email)) {
            return random_token();
        }
        let now = now_ms();
        let new_token = random_token()?;
        // RETURNING token: the freshly minted value on INSERT, or the row's
        // existing (never-rotated) token on the email-conflict UPDATE.
        let row: (i64, String) = sqlx::query_as(
            "INSERT INTO users (name, email, install_id, version, os_version, country, device_id, ip, token, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(email) DO UPDATE SET
               name = excluded.name,
               install_id = excluded.install_id,
               version = excluded.version,
               os_version = excluded.os_version,
               country = COALESCE(excluded.country, users.country),
               device_id = COALESCE(excluded.device_id, users.device_id),
               ip = COALESCE(excluded.ip, users.ip),
               updated_at = excluded.updated_at
             RETURNING id, token",
        )
        .bind(&u.name)
        .bind(&u.email)
        .bind(&u.install_id)
        .bind(&u.version)
        .bind(&u.os_version)
        .bind(&u.country)
        .bind(&u.device_id)
        .bind(&u.ip)
        .bind(&new_token)
        .bind(now)
        .bind(now)
        .fetch_one(&self.pool)
        .await?;

        // Phase 1 attribution: if the client forwarded the landing `iid`, map
        // this install to it (first mapping wins — OR IGNORE on the install_id
        // PK). Only reached for non-excluded installs, so smoke/dev never map.
        // yagni: explicit-iid handoff only; an IP+UA fuzzy fallback is possible
        // later if clients can't forward the iid, but that's privacy-sensitive.
        if let Some(iid) = u.iid.as_deref().filter(|s| !s.is_empty()) {
            sqlx::query(
                "INSERT OR IGNORE INTO install_landing_map (install_id, iid, matched_at) VALUES (?, ?, ?)",
            )
            .bind(&u.install_id)
            .bind(iid)
            .bind(now)
            .execute(&self.pool)
            .await?;
        }
        // The bearer is a SIGNED token so the Pro relay — now its own service —
        // can identify the caller offline instead of asking us on every request.
        // Minted after the insert because the claims carry the row id.
        //
        // Re-registration replaces a legacy opaque token or a JWT near expiry;
        // an already-valid JWT is returned untouched, preserving the old
        // "re-registering never rotates your token" behavior for the common
        // case. That is also the migration: a shipped client whose old token
        // Pro rejects re-registers here and walks away with a JWT.
        let (user_id, existing) = row;
        let token = match crate::jwt::verify(&self.jwt_secret, &existing, now_secs()) {
            Ok(_) => existing,
            Err(_) => match crate::jwt::mint(
                &self.jwt_secret,
                user_id,
                &u.email,
                u.device_id.as_deref(),
                now_secs(),
            ) {
                Some(minted) => {
                    sqlx::query("UPDATE users SET token = ? WHERE id = ?")
                        .bind(&minted)
                        .bind(user_id)
                        .execute(&self.pool)
                        .await?;
                    minted
                }
                // No signing secret configured — keep the random token so the
                // service still works, just without offline verification.
                None => existing,
            },
        };
        Ok(token)
    }

    /// Overwrite a row's token. Test-only seam for the signed-token migration
    /// case, which needs a row holding a pre-migration opaque token.
    #[doc(hidden)]
    pub async fn set_token_for_test(&self, user_id: i64, token: &str) -> Result<(), sqlx::Error> {
        sqlx::query("UPDATE users SET token = ? WHERE id = ?")
            .bind(token)
            .bind(user_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Resolve a bearer token to its user row, or `None` for an empty/unknown
    /// token.
    ///
    /// Prefer a verified JWT (Pro mints these; `sub` is the user id) and stub
    /// the `users` row so reports still join. Fall back to the opaque
    /// `users.token` string match for leftovers that have not re-registered.
    pub async fn get_user_by_token(&self, token: &str) -> Result<Option<UserRow>, sqlx::Error> {
        if token.is_empty() {
            return Ok(None);
        }
        if let Ok(claims) = crate::jwt::verify(&self.jwt_secret, token, now_secs()) {
            return self.upsert_user_from_jwt(&claims, token).await.map(Some);
        }
        let row = sqlx::query_as::<_, UserRow>(
            "SELECT id, install_id, email,
                    subscription_stripe_id, subscription_status, subscription_expires
               FROM users WHERE token = ?",
        )
        .bind(token)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Insert or refresh the telemetry `users` row from a verified Pro JWT.
    /// Id collision with a different email uses the existing email row so
    /// UNIQUE(email) is never broken.
    async fn upsert_user_from_jwt(
        &self,
        claims: &crate::jwt::Claims,
        token: &str,
    ) -> Result<UserRow, sqlx::Error> {
        let now = now_ms();
        let email = claims.email.trim().to_lowercase();
        if let Some(row) = self.get_user_by_id(claims.user_id).await? {
            sqlx::query(
                "UPDATE users SET email = ?, device_id = COALESCE(?, device_id),
                        token = ?, updated_at = ? WHERE id = ?",
            )
            .bind(&email)
            .bind(claims.device_id.as_deref())
            .bind(token)
            .bind(now)
            .bind(claims.user_id)
            .execute(&self.pool)
            .await?;
            return Ok(row);
        }
        if let Some(row) = self.get_user_by_email(&email).await? {
            sqlx::query(
                "UPDATE users SET device_id = COALESCE(?, device_id),
                        token = ?, updated_at = ? WHERE id = ?",
            )
            .bind(claims.device_id.as_deref())
            .bind(token)
            .bind(now)
            .bind(row.id)
            .execute(&self.pool)
            .await?;
            return self.get_user_by_id(row.id).await?.ok_or(sqlx::Error::RowNotFound);
        }
        sqlx::query(
            "INSERT INTO users (id, name, email, install_id, device_id, token, created_at, updated_at)
             VALUES (?, '', ?, '', ?, ?, ?, ?)",
        )
        .bind(claims.user_id)
        .bind(&email)
        .bind(claims.device_id.as_deref())
        .bind(token)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        self.get_user_by_id(claims.user_id)
            .await?
            .ok_or(sqlx::Error::RowNotFound)
    }

    /// Resolve a user by primary key (Checkout `client_reference_id`).
    pub async fn get_user_by_id(&self, id: i64) -> Result<Option<UserRow>, sqlx::Error> {
        let row = sqlx::query_as::<_, UserRow>(
            "SELECT id, install_id, email,
                    subscription_stripe_id, subscription_status, subscription_expires
               FROM users WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Resolve a user by email (Stripe customer email path).
    pub async fn get_user_by_email(&self, email: &str) -> Result<Option<UserRow>, sqlx::Error> {
        let row = sqlx::query_as::<_, UserRow>(
            "SELECT id, install_id, email,
                    subscription_stripe_id, subscription_status, subscription_expires
               FROM users WHERE lower(email) = lower(?)",
        )
        .bind(email)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Resolve by Stripe customer id (subscription.updated after first checkout).
    pub async fn get_user_by_stripe_customer(
        &self,
        stripe_id: &str,
    ) -> Result<Option<UserRow>, sqlx::Error> {
        let row = sqlx::query_as::<_, UserRow>(
            "SELECT id, install_id, email,
                    subscription_stripe_id, subscription_status, subscription_expires
               FROM users WHERE subscription_stripe_id = ?",
        )
        .bind(stripe_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    /// Persist Stripe subscription entitlement on a user (webhook fulfillment).
    pub async fn set_user_subscription(
        &self,
        user_id: i64,
        stripe_id: &str,
        status: &str,
        expires: Option<i64>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE users SET
               subscription_stripe_id = ?,
               subscription_status = ?,
               subscription_expires = ?,
               updated_at = ?
             WHERE id = ?",
        )
        .bind(stripe_id)
        .bind(status)
        .bind(expires)
        .bind(now_ms())
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Open a refund request. Returns the new row id.
    pub async fn insert_refund_request(&self, r: &RefundRequestInput<'_>) -> Result<i64, sqlx::Error> {
        let res = sqlx::query(
            "INSERT INTO pro_refund_requests
               (user_id, email, customer, subscription_id, status,
                requests_today, tokens_today, total_requests, total_tokens, created_at)
             VALUES (?, ?, ?, ?, 'pending', ?, ?, ?, ?, ?)",
        )
        .bind(r.user_id)
        .bind(r.email)
        .bind(r.customer)
        .bind(r.subscription_id)
        .bind(r.requests_today)
        .bind(r.tokens_today)
        .bind(r.total_requests)
        .bind(r.total_tokens)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(res.last_insert_rowid())
    }

    /// The user's open (pending) request, if any — one at a time.
    pub async fn pending_refund_request(&self, user_id: i64) -> Result<Option<Value>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT * FROM pro_refund_requests WHERE user_id = ? AND status = 'pending' ORDER BY id DESC LIMIT 1",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|r| refund_request_json(&r)))
    }

    pub async fn get_refund_request(&self, id: i64) -> Result<Option<Value>, sqlx::Error> {
        let row = sqlx::query("SELECT * FROM pro_refund_requests WHERE id = ?")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|r| refund_request_json(&r)))
    }

    /// Requests by status (`None` = all), newest first, capped at 200.
    pub async fn list_refund_requests(&self, status: Option<&str>) -> Result<Vec<Value>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT * FROM pro_refund_requests
             WHERE (? IS NULL OR status = ?)
             ORDER BY id DESC LIMIT 200",
        )
        .bind(status)
        .bind(status)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(refund_request_json).collect())
    }

    /// Mark a request approved/denied (only from pending). Returns false if the
    /// row was not pending — a double-approve must not refund twice.
    pub async fn resolve_refund_request(&self, id: i64, status: &str, note: Option<&str>) -> Result<bool, sqlx::Error> {
        let res = sqlx::query(
            "UPDATE pro_refund_requests SET status = ?, note = ?, resolved_at = ?
             WHERE id = ? AND status = 'pending'",
        )
        .bind(status)
        .bind(note)
        .bind(now_ms())
        .bind(id)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Record a Stripe webhook event id. Returns true if newly inserted, false
    /// if the event was already processed (idempotent skip).
    pub async fn insert_stripe_webhook_event(
        &self,
        event_id: &str,
        event_type: &str,
    ) -> Result<bool, sqlx::Error> {
        let res = sqlx::query(
            "INSERT OR IGNORE INTO stripe_webhook_events (event_id, event_type, created_at)
             VALUES (?, ?, ?)",
        )
        .bind(event_id)
        .bind(event_type)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected() > 0)
    }

    /// Link a signup to the install pinging with its bearer, when the row has
    /// no install yet. Since 2026-09-14 `/api/register` lives on dottie-pro, so
    /// rows here are stubbed from the JWT (which carries no install id) with
    /// `install_id = ''`, and every signup-keyed report lost its install. The
    /// first authenticated ping/event fills it. Fill-only: an existing link is
    /// never moved, so a second Mac on the same email does not flip-flop it.
    pub async fn link_install_if_unlinked(
        &self,
        user_id: i64,
        install_id: &str,
    ) -> Result<bool, sqlx::Error> {
        if install_id.is_empty() || self.is_excluded_ingest(Some(install_id), None) {
            return Ok(false);
        }
        let done = sqlx::query(
            "UPDATE users SET install_id = ?, updated_at = ?
              WHERE id = ? AND (install_id IS NULL OR install_id = '')",
        )
        .bind(install_id)
        .bind(now_ms())
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// Record an app-launch ping. Excluded installs are dropped.
    pub async fn record_ping(&self, p: &Ping) -> Result<(), sqlx::Error> {
        if self.is_excluded_ingest(Some(&p.install_id), None) {
            return Ok(());
        }
        sqlx::query("INSERT INTO pings (install_id, version, created_at) VALUES (?, ?, ?)")
            .bind(&p.install_id)
            .bind(&p.version)
            .bind(now_ms())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Record a product event. Excluded installs are dropped. When a non-empty
    /// `event_id` is supplied, use INSERT OR IGNORE against the partial unique
    /// index so at-least-once retries never double-count; otherwise a plain
    /// always-insert.
    pub async fn record_event(&self, e: &Event) -> Result<(), sqlx::Error> {
        if self.is_excluded_ingest(Some(&e.install_id), None) {
            return Ok(());
        }
        // One first-success per install: later ones are retries or a client
        // that lost its once-flag, and would only blur time-to-first-success.
        if e.name == crate::variant::FIRST_SUCCESS {
            let seen: Option<i64> = sqlx::query_scalar(
                "SELECT 1 FROM events WHERE install_id = ? AND name = ? LIMIT 1",
            )
            .bind(&e.install_id)
            .bind(&e.name)
            .fetch_optional(&self.pool)
            .await?;
            if seen.is_some() {
                return Ok(());
            }
        }
        let props_json = json_or_null(&with_variant(&e.props, &e.install_id));
        let now = now_ms();
        // Track whether a row was actually written: an OR IGNORE that hits the
        // event_id unique index affects 0 rows (a deduped retry), and MUST NOT
        // bump the issue count — otherwise at-least-once retries inflate it.
        let inserted = match &e.event_id {
            Some(eid) if !eid.is_empty() => {
                sqlx::query(
                    "INSERT OR IGNORE INTO events (install_id, name, url, props, version, created_at, event_id)
                     VALUES (?, ?, ?, ?, ?, ?, ?)",
                )
                .bind(&e.install_id)
                .bind(&e.name)
                .bind(&e.url)
                .bind(&props_json)
                .bind(&e.version)
                .bind(now)
                .bind(eid)
                .execute(&self.pool)
                .await?
                .rows_affected()
                    > 0
            }
            _ => {
                sqlx::query(
                    "INSERT INTO events (install_id, name, url, props, version, created_at)
                     VALUES (?, ?, ?, ?, ?, ?)",
                )
                .bind(&e.install_id)
                .bind(&e.name)
                .bind(&e.url)
                .bind(&props_json)
                .bind(&e.version)
                .bind(now)
                .execute(&self.pool)
                .await?;
                true
            }
        };

        // Phase 1: roll `error.%` events up into the issues lifecycle table.
        if inserted && e.name.starts_with("error.") {
            self.upsert_issue(&e.name, &e.props, &e.version, now).await?;
        }
        Ok(())
    }

    /// Upsert an error signature into `issues`.
    /// Fingerprint:
    /// - `error.thrown` (and anything with file/function/line): `name|file|function|line`
    /// - structured errors (RSE, gateway_buffered, …): `name|code|reason|provider`
    ///   so `error.realtime_server_error` splits into llm_error/xai vs voice mint etc.
    ///   instead of one mega-row with empty file fields.
    /// - `error.gateway_buffered` sends `category`/`reason`/`component` (no
    ///   `code`/`provider`). Empty code/provider fall back to those so
    ///   `reason=connection` does not collapse TTS/LLM/STT into one row.
    async fn upsert_issue(
        &self,
        name: &str,
        props: &Option<Value>,
        version: &Option<String>,
        now: i64,
    ) -> Result<(), sqlx::Error> {
        let file = prop_str(props, "file");
        let function = prop_str(props, "function");
        let line = prop_str(props, "line");
        let nonempty = |k: &str| prop_str(props, k).filter(|s| !s.is_empty());
        // RSE uses code/provider; gateway_buffered uses category/component.
        let code = nonempty("code").or_else(|| nonempty("category"));
        let reason = nonempty("reason");
        let provider = nonempty("provider").or_else(|| nonempty("component"));
        // Prefer Swift location when present; else structured props for grouping.
        let has_site = file.as_ref().is_some_and(|s| !s.is_empty())
            || function.as_ref().is_some_and(|s| !s.is_empty())
            || line.as_ref().is_some_and(|s| !s.is_empty());
        let (disp_file, disp_fn, disp_line) = if has_site {
            (file, function, line)
        } else {
            // Store human-readable slices in issues columns for the dashboard.
            (
                code.clone(),
                reason.clone(),
                provider.clone(),
            )
        };
        let fingerprint = format!(
            "{name}|{}|{}|{}",
            disp_file.as_deref().unwrap_or(""),
            disp_fn.as_deref().unwrap_or(""),
            disp_line.as_deref().unwrap_or(""),
        );
        // A resolved issue only regresses when the sighting comes from a build
        // at or after the fix. Old builds still in the field keep bumping
        // count/last_seen but must not reopen the issue. An issue resolved
        // without a version, or an event with no version, keeps the old
        // always-regress behaviour (we can't prove the report is stale).
        let prior: Option<(String, Option<String>)> =
            sqlx::query_as("SELECT status, resolved_in_version FROM issues WHERE fingerprint = ?")
                .bind(&fingerprint)
                .fetch_optional(&self.pool)
                .await?;
        let regress = match prior {
            Some((status, Some(fixed_in))) if status == "resolved" => version
                .as_deref()
                .is_some_and(|v| version_cmp(v, &fixed_in) != std::cmp::Ordering::Less),
            _ => true,
        };
        sqlx::query(
            "INSERT INTO issues (fingerprint, name, file, function, line, status, count, first_seen, last_seen, last_version)
             VALUES (?, ?, ?, ?, ?, 'active', 1, ?, ?, ?)
             ON CONFLICT(fingerprint) DO UPDATE SET
               count        = count + 1,
               last_seen    = excluded.last_seen,
               last_version = COALESCE(excluded.last_version, issues.last_version),
               status       = CASE WHEN issues.status = 'resolved' AND ? = 1 THEN 'regressed' ELSE issues.status END,
               resolved_at  = CASE WHEN issues.status = 'resolved' AND ? = 1 THEN NULL ELSE issues.resolved_at END",
        )
        .bind(&fingerprint)
        .bind(name)
        .bind(&disp_file)
        .bind(&disp_fn)
        .bind(&disp_line)
        .bind(now)
        .bind(now)
        .bind(version)
        .bind(i64::from(regress))
        .bind(i64::from(regress))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Set an issue's status by id (operator action from the dashboard/PATCH).
    /// Only `active`/`resolved` are accepted from callers; `resolved` stamps
    /// resolved_at, `active` clears it. `regressed` is set only by the ingest
    /// upsert, never here. Returns the updated issue row, or None if id unknown.
    ///
    /// `version` is the build the fix ships in: stored on resolve so later
    /// sightings from OLDER builds don't flip the issue to `regressed`. Cleared
    /// when reopening (`active`) or when resolving without a version.
    pub async fn set_issue_status(
        &self,
        id: i64,
        status: &str,
        version: Option<&str>,
    ) -> Result<Option<Value>, sqlx::Error> {
        let resolved_at = if status == "resolved" { Some(now_ms()) } else { None };
        let resolved_in = if status == "resolved" { version } else { None };
        let affected = sqlx::query(
            "UPDATE issues SET status = ?, resolved_at = ?, resolved_in_version = ? WHERE id = ?",
        )
        .bind(status)
        .bind(resolved_at)
        .bind(resolved_in)
        .bind(id)
        .execute(&self.pool)
        .await?
        .rows_affected();
        if affected == 0 {
            return Ok(None);
        }
        self.get_issue_by_id(id).await
    }

    /// Record a redacted diagnostic bundle. Excluded installs are dropped. The
    /// three JSON columns serialize to TEXT (null when absent); note is clamped
    /// to 500 chars.
    pub async fn record_diagnostic(&self, d: &Diagnostic) -> Result<(), sqlx::Error> {
        if self.is_excluded_ingest(Some(&d.install_id), None) {
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO diagnostics (install_id, version, fingerprint, logs, gateway, note, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&d.install_id)
        .bind(&d.version)
        .bind(json_or_null(&d.fingerprint))
        .bind(json_or_null(&d.logs))
        .bind(json_or_null(&d.gateway))
        .bind(clamp_opt(&d.note, 500))
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Upsert a landing-page visit keyed on `iid`. On conflict, ip/ua/path and
    /// last_seen_at overwrite, but referrer + all UTM fields use COALESCE so the
    /// FIRST non-null value is preserved.
    pub async fn record_landing_visit(&self, v: &LandingVisit) -> Result<(), sqlx::Error> {
        let now = now_ms();
        sqlx::query(
            "INSERT INTO landing_visits (iid, ip, ua, path, referrer, utm_source, utm_medium, utm_campaign, utm_term, utm_content, first_seen_at, last_seen_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(iid) DO UPDATE SET
               ip            = excluded.ip,
               ua            = excluded.ua,
               path          = excluded.path,
               referrer      = COALESCE(excluded.referrer, referrer),
               utm_source    = COALESCE(excluded.utm_source, utm_source),
               utm_medium    = COALESCE(excluded.utm_medium, utm_medium),
               utm_campaign  = COALESCE(excluded.utm_campaign, utm_campaign),
               utm_term      = COALESCE(excluded.utm_term, utm_term),
               utm_content   = COALESCE(excluded.utm_content, utm_content),
               last_seen_at  = excluded.last_seen_at",
        )
        .bind(&v.iid)
        .bind(&v.ip)
        .bind(&v.ua)
        .bind(&v.path)
        .bind(&v.referrer)
        .bind(&v.utm_source)
        .bind(&v.utm_medium)
        .bind(&v.utm_campaign)
        .bind(&v.utm_term)
        .bind(&v.utm_content)
        .bind(now)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Append a named landing-page event. Unlike `record_landing_visit` this is
    /// insert-only (one row per event, not an upsert) — the visitor's event
    /// stream. `props` serializes to TEXT (null when absent).
    pub async fn record_landing_event(&self, e: &LandingEvent) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO landing_events (iid, name, props, path, ip, ua, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&e.iid)
        .bind(&e.name)
        .bind(json_or_null(&e.props))
        .bind(&e.path)
        .bind(&e.ip)
        .bind(&e.ua)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Purge all landing data for one `iid`: its events + its visit row. Returns
    /// `(events_deleted, visits_deleted)`. Admin data-hygiene / per-visitor
    /// delete (privacy request or probe cleanup). Leaves `install_landing_map`
    /// alone — that ties to a real install/user; a purged iid typically has none.
    pub async fn delete_landing_iid(&self, iid: &str) -> Result<(i64, i64), sqlx::Error> {
        let events = sqlx::query("DELETE FROM landing_events WHERE iid = ?")
            .bind(iid)
            .execute(&self.pool)
            .await?
            .rows_affected() as i64;
        let visits = sqlx::query("DELETE FROM landing_visits WHERE iid = ?")
            .bind(iid)
            .execute(&self.pool)
            .await?
            .rows_affected() as i64;
        Ok((events, visits))
    }

    /// Upsert a site (multi-tenant). New sites get a random `write_key` (for the
    /// future `/collect` ingest); existing sites keep their key, only name/domain
    /// refresh. `id` reuses Umami's website_id so imports are idempotent.
    pub async fn upsert_site(
        &self,
        id: &str,
        name: &str,
        domain: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO sites (id, name, domain, write_key, created_at)
             VALUES (?, ?, ?, lower(hex(randomblob(16))), ?)
             ON CONFLICT(id) DO UPDATE SET
               name   = excluded.name,
               domain = excluded.domain",
        )
        .bind(id)
        .bind(name)
        .bind(domain)
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Create (or refresh name/domain for) a site and return the full row incl. write_key.
    /// When `id` is None/empty, generates a random 32-char hex id.
    pub async fn create_site(
        &self,
        name: &str,
        domain: Option<&str>,
        id: Option<&str>,
    ) -> Result<serde_json::Value, sqlx::Error> {
        let id = match id.map(str::trim).filter(|s| !s.is_empty()) {
            Some(s) => s.to_string(),
            None => {
                sqlx::query_scalar::<_, String>("SELECT lower(hex(randomblob(16)))")
                    .fetch_one(self.pool())
                    .await?
            }
        };
        self.upsert_site(&id, name, domain).await?;
        let row = sqlx::query(
            "SELECT id, name, domain, write_key, created_at FROM sites WHERE id = ?",
        )
        .bind(&id)
        .fetch_one(self.pool())
        .await?;
        let domain: Option<String> = row.get("domain");
        Ok(serde_json::json!({
            "id": row.get::<String, _>("id"),
            "name": row.get::<String, _>("name"),
            "domain": domain,
            "write_key": row.get::<String, _>("write_key"),
            "created_at": row.get::<i64, _>("created_at"),
            "events": 0,
            "visitors": 0,
            "last_event": null,
        }))
    }

    /// Resolve the tracker `website` field to a site id. Accepts the public
    /// `write_key` or the site id itself (the Umami website UUID on pages that
    /// still post through an Umami-compatible proxy). Returns None when nothing matches
    /// (handler → 404).
    pub async fn site_by_write_key(&self, write_key: &str) -> Result<Option<String>, sqlx::Error> {
        let id: Option<String> = sqlx::query_scalar(
            "SELECT id FROM sites WHERE write_key = ? OR id = ? LIMIT 1",
        )
        .bind(write_key)
        .bind(write_key)
        .fetch_optional(&self.pool)
        .await?;
        Ok(id)
    }

    /// The most recent `session_id` for a visitor on a site whose last event is
    /// at/after `since_ms` (the 30-min inactivity cutoff). `Some` → continue that
    /// session; `None` → start a new one. This is what makes visits/bounce
    /// correct for the live collect path.
    pub async fn latest_session_for(
        &self,
        site_id: &str,
        distinct_id: &str,
        since_ms: i64,
    ) -> Result<Option<String>, sqlx::Error> {
        let sid: Option<String> = sqlx::query_scalar(
            "SELECT session_id FROM site_events
             WHERE site_id = ? AND distinct_id = ? AND created_at >= ?
               AND session_id IS NOT NULL
             ORDER BY created_at DESC LIMIT 1",
        )
        .bind(site_id)
        .bind(distinct_id)
        .bind(since_ms)
        .fetch_optional(&self.pool)
        .await?;
        Ok(sid)
    }

    /// Bulk-insert generic site events in one transaction. Returns the count
    /// inserted. Used by the admin import endpoint (historical backfill) and the
    /// live `/collect` path. `created_at` comes from each `SiteEvent` (not
    /// stamped here), so imported rows keep their original timestamps.
    pub async fn insert_site_events_bulk(
        &self,
        site_id: &str,
        events: &[SiteEvent],
    ) -> Result<i64, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        for e in events {
            sqlx::query(
                "INSERT INTO site_events
                   (site_id, event_type, name, url, referrer, distinct_id, session_id,
                    hostname, browser, os, device, country,
                    utm_source, utm_medium, utm_campaign, props, created_at, ua, bot)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(site_id)
            .bind(e.event_type)
            .bind(&e.name)
            .bind(&e.url)
            .bind(&e.referrer)
            .bind(&e.distinct_id)
            .bind(&e.session_id)
            .bind(&e.hostname)
            .bind(&e.browser)
            .bind(&e.os)
            .bind(&e.device)
            .bind(&e.country)
            .bind(&e.utm_source)
            .bind(&e.utm_medium)
            .bind(&e.utm_campaign)
            .bind(json_or_null(&e.props))
            .bind(e.created_at)
            .bind(&e.ua)
            .bind(e.bot)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(events.len() as i64)
    }

    /// Import path. Skips a row when `source_event_id` is already stored for the
    /// site, or when the natural key already exists: site + `created_at` +
    /// `event_type` + session + url + name. Existing rows from before
    /// `source_event_id` existed match on that key; if the new row carries an
    /// id, it is stamped onto one matching row so the next retry hits the
    /// unique index. Live `/api/collect` does not use this — identical live
    /// events still insert. Returns the number of rows actually inserted.
    pub async fn insert_site_events_deduped(
        &self,
        site_id: &str,
        events: &[SiteEvent],
    ) -> Result<i64, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let mut inserted = 0i64;
        for e in events {
            let src = e
                .source_event_id
                .as_deref()
                .filter(|s| !s.is_empty());
            if let Some(src) = src {
                let already: Option<i64> = sqlx::query_scalar(
                    "SELECT 1 FROM site_events WHERE site_id = ? AND source_event_id = ? LIMIT 1",
                )
                .bind(site_id)
                .bind(src)
                .fetch_optional(&mut *tx)
                .await?;
                if already.is_some() {
                    continue;
                }
                let stamped = sqlx::query(
                    "UPDATE site_events SET source_event_id = ?
                     WHERE id = (
                       SELECT id FROM site_events
                       WHERE site_id = ?
                         AND source_event_id IS NULL
                         AND created_at = ?
                         AND event_type = ?
                         AND ifnull(session_id, '') = ifnull(?, '')
                         AND ifnull(url, '') = ifnull(?, '')
                         AND ifnull(name, '') = ifnull(?, '')
                       ORDER BY id
                       LIMIT 1
                     )",
                )
                .bind(src)
                .bind(site_id)
                .bind(e.created_at)
                .bind(e.event_type)
                .bind(&e.session_id)
                .bind(&e.url)
                .bind(&e.name)
                .execute(&mut *tx)
                .await?;
                if stamped.rows_affected() > 0 {
                    continue;
                }
            }
            let wrote = sqlx::query(
                "INSERT INTO site_events
                   (site_id, event_type, name, url, referrer, distinct_id, session_id,
                    hostname, browser, os, device, country,
                    utm_source, utm_medium, utm_campaign, props, created_at, ua, bot,
                    source_event_id)
                 SELECT ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?
                 WHERE NOT EXISTS (
                   SELECT 1 FROM site_events
                   WHERE site_id = ?
                     AND created_at = ?
                     AND event_type = ?
                     AND ifnull(session_id, '') = ifnull(?, '')
                     AND ifnull(url, '') = ifnull(?, '')
                     AND ifnull(name, '') = ifnull(?, '')
                 )",
            )
            .bind(site_id)
            .bind(e.event_type)
            .bind(&e.name)
            .bind(&e.url)
            .bind(&e.referrer)
            .bind(&e.distinct_id)
            .bind(&e.session_id)
            .bind(&e.hostname)
            .bind(&e.browser)
            .bind(&e.os)
            .bind(&e.device)
            .bind(&e.country)
            .bind(&e.utm_source)
            .bind(&e.utm_medium)
            .bind(&e.utm_campaign)
            .bind(json_or_null(&e.props))
            .bind(e.created_at)
            .bind(&e.ua)
            .bind(e.bot)
            .bind(src)
            .bind(site_id)
            .bind(e.created_at)
            .bind(e.event_type)
            .bind(&e.session_id)
            .bind(&e.url)
            .bind(&e.name)
            .execute(&mut *tx)
            .await?;
            inserted += wrote.rows_affected() as i64;
        }
        tx.commit().await?;
        Ok(inserted)
    }

    /// Record a rejected registration attempt (validation/rate-limit failures).
    /// Per-field length clamps match db.js exactly: name 200, email 320,
    /// install_id 200, version 50, os_version 100, ua 300. country/ip pass
    /// through unclamped; reason is required.
    pub async fn record_rejected_registration(
        &self,
        r: &RejectedRegistration,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO rejected_registrations
               (name, email, install_id, version, os_version, country, reason, ip, ua, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(clamp_opt(&r.name, 200))
        .bind(clamp_opt(&r.email, 320))
        .bind(clamp_opt(&r.install_id, 200))
        .bind(clamp_opt(&r.version, 50))
        .bind(clamp_opt(&r.os_version, 100))
        .bind(&r.country)
        .bind(&r.reason)
        .bind(&r.ip)
        .bind(clamp_opt(&r.ua, 300))
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // ---- Dottie Pro relay quota -------------------------------------------

    /// Today's `(requests, tokens)` for a user, `(0, 0)` when the user hasn't
    /// used the relay today. The day boundary is UTC (SQLite's `'now'`).
    pub async fn pro_usage_today(&self, user_id: i64) -> Result<(i64, i64), sqlx::Error> {
        let row = sqlx::query_as::<_, (i64, i64)>(
            "SELECT requests, tokens FROM pro_usage
              WHERE user_id = ? AND day = strftime('%Y-%m-%d','now')",
        )
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.unwrap_or((0, 0)))
    }

    /// Count one relay request against today's quota and return the POST-increment
    /// `(requests, tokens)`. Upsert + RETURNING keeps the read-modify-write in a
    /// single statement, so two concurrent requests can't both read the same
    /// pre-increment count and slip past the cap.
    pub async fn pro_count_request(&self, user_id: i64) -> Result<(i64, i64), sqlx::Error> {
        let row = sqlx::query_as::<_, (i64, i64)>(
            "INSERT INTO pro_usage (user_id, day, requests, tokens)
             VALUES (?, strftime('%Y-%m-%d','now'), 1, 0)
             ON CONFLICT(user_id, day) DO UPDATE SET requests = requests + 1
             RETURNING requests, tokens",
        )
        .bind(user_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// Today's `(requests, tokens)` summed across ALL users — the global spend
    /// breaker. Per-user caps bound one account; this bounds the whole service,
    /// so scripted registrations can't multiply the daily spend past a known
    /// ceiling.
    pub async fn pro_global_usage_today(&self) -> Result<(i64, i64), sqlx::Error> {
        let row = sqlx::query_as::<_, (i64, i64)>(
            "SELECT COALESCE(SUM(requests), 0), COALESCE(SUM(tokens), 0)
               FROM pro_usage WHERE day = strftime('%Y-%m-%d','now')",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// `(requests, tokens)` for one user over the last `days` UTC days
    /// (inclusive of today). Paid rolling window. `days` is interpolated as
    /// an integer into a `-{n} days` modifier (not user input).
    pub async fn pro_user_usage_days(&self, user_id: i64, days: i64) -> Result<(i64, i64), sqlx::Error> {
        let days = days.max(1);
        let sql = format!(
            "SELECT COALESCE(SUM(requests), 0), COALESCE(SUM(tokens), 0)
               FROM pro_usage
              WHERE user_id = ?
                AND day >= strftime('%Y-%m-%d', 'now', '-{days} days')"
        );
        let row = sqlx::query_as::<_, (i64, i64)>(&sql)
            .bind(user_id)
            .fetch_one(&self.pool)
            .await?;
        Ok(row)
    }

    /// Lifetime `(requests, tokens)` across ALL users and ALL days — the hard
    /// spend ceiling's input. pro_usage rows are never deleted, so this is the
    /// true total since the relay went live.
    pub async fn pro_total_usage(&self) -> Result<(i64, i64), sqlx::Error> {
        let row = sqlx::query_as::<_, (i64, i64)>(
            "SELECT COALESCE(SUM(requests), 0), COALESCE(SUM(tokens), 0) FROM pro_usage",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// Lifetime `(requests, tokens)` behind the free-credit ceiling. Summed
    /// across EVERY account sharing the user's `device_id` — re-registering
    /// with a fresh email on the same Mac inherits the machine's spent credit
    /// instead of minting a new dollar. A user with no device_id (old client)
    /// falls back to their own rows; the UNION keeps the user's own id in the
    /// set in both cases. Never resets.
    pub async fn pro_user_total_usage(&self, user_id: i64) -> Result<(i64, i64), sqlx::Error> {
        let row = sqlx::query_as::<_, (i64, i64)>(
            "SELECT COALESCE(SUM(requests), 0), COALESCE(SUM(tokens), 0)
               FROM pro_usage
              WHERE user_id IN (
                    SELECT u2.id FROM users u1
                      JOIN users u2 ON u2.device_id = u1.device_id
                     WHERE u1.id = ? AND u1.device_id IS NOT NULL AND u1.device_id != ''
                    UNION SELECT ?)",
        )
        .bind(user_id)
        .bind(user_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// Append one row to the audit ledger. Awaited by callers on the reject path
    /// and after settle on the success path — an audit trail that silently drops
    /// rows under load is not an audit trail. Failure is logged, never fatal:
    /// losing the ledger must not take the relay down.
    pub async fn record_pro_call(&self, c: &ProCall<'_>) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO pro_calls
               (user_id, email, route, model, outcome, reason, status,
                prompt_tokens, completion_tokens, tokens, estimated, seconds,
                paid, ip, cost_ticks, cost_estimated, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(c.user_id)
        .bind(c.email)
        .bind(c.route)
        .bind(c.model)
        .bind(c.outcome)
        .bind(c.reason)
        .bind(c.status)
        .bind(c.prompt_tokens)
        .bind(c.completion_tokens)
        .bind(c.tokens)
        .bind(i64::from(c.estimated))
        .bind(c.seconds)
        .bind(i64::from(c.paid))
        .bind(c.ip)
        .bind(c.cost_ticks)
        .bind(i64::from(c.cost_estimated))
        .bind(now_ms())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Ledger slice, newest first. Every filter is optional so one endpoint
    /// serves "what did this user do", "what happened on this route", and
    /// "what was refused and why".
    pub async fn list_pro_calls(
        &self,
        user_id: Option<i64>,
        route: Option<&str>,
        outcome: Option<&str>,
        since_ms: i64,
        limit: i64,
    ) -> Result<Vec<Value>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT id, user_id, email, route, model, outcome, reason, status,
                    prompt_tokens, completion_tokens, tokens, estimated, seconds,
                    paid, ip, cost_ticks, cost_estimated, created_at
               FROM pro_calls
              WHERE created_at >= ?
                AND (? IS NULL OR user_id = ?)
                AND (? IS NULL OR route = ?)
                AND (? IS NULL OR outcome = ?)
              ORDER BY id DESC
              LIMIT ?",
        )
        .bind(since_ms)
        .bind(user_id)
        .bind(user_id)
        .bind(route)
        .bind(route)
        .bind(outcome)
        .bind(outcome)
        .bind(limit.clamp(1, 1000))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows.iter().map(pro_call_json).collect())
    }

    /// Ledger rolled up by route: calls, billed tokens, and how much of that was
    /// a synthetic audio estimate rather than a real token count.
    pub async fn pro_calls_by_route(&self, since_ms: i64) -> Result<Vec<Value>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT route, outcome,
                    COUNT(*) AS calls,
                    COALESCE(SUM(tokens), 0) AS tokens,
                    COALESCE(SUM(CASE WHEN estimated = 1 THEN tokens ELSE 0 END), 0) AS estimated_tokens,
                    COALESCE(SUM(prompt_tokens), 0) AS prompt_tokens,
                    COALESCE(SUM(completion_tokens), 0) AS completion_tokens,
                    COALESCE(SUM(cost_ticks), 0) AS cost_ticks,
                    COALESCE(SUM(CASE WHEN cost_estimated = 1 THEN cost_ticks ELSE 0 END), 0) AS estimated_cost_ticks
               FROM pro_calls
              WHERE created_at >= ?
              GROUP BY route, outcome
              ORDER BY tokens DESC",
        )
        .bind(since_ms)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                json!({
                    "route": r.get::<String, _>("route"),
                    "outcome": r.get::<String, _>("outcome"),
                    "calls": r.get::<i64, _>("calls"),
                    "tokens": r.get::<i64, _>("tokens"),
                    "estimated_tokens": r.get::<i64, _>("estimated_tokens"),
                    "prompt_tokens": r.get::<i64, _>("prompt_tokens"),
                    "completion_tokens": r.get::<i64, _>("completion_tokens"),
                    "cost_usd": ticks_to_usd(r.get::<i64, _>("cost_ticks")),
                    "estimated_cost_usd": ticks_to_usd(r.get::<i64, _>("estimated_cost_ticks")),
                })
            })
            .collect())
    }

    /// Top Pro consumers: lifetime requests/tokens per user, joined to the
    /// account so an operator can name who is spending the fleet allowance.
    /// Nothing exposed `pro_usage` per user before — an abuse report could say
    /// the fleet cap was hit but not by whom.
    pub async fn pro_top_users(&self, limit: i64) -> Result<Vec<Value>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT u.id, u.email, u.country, u.device_id, u.created_at,
                    u.subscription_status,
                    COALESCE(SUM(p.requests), 0)   AS requests,
                    COALESCE(SUM(p.tokens), 0)     AS tokens,
                    COALESCE(SUM(p.cost_ticks), 0) AS cost_ticks,
                    MAX(p.day)                     AS last_day
               FROM pro_usage p JOIN users u ON u.id = p.user_id
              GROUP BY u.id
              ORDER BY cost_ticks DESC, tokens DESC
              LIMIT ?",
        )
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                json!({
                    "user_id": r.get::<i64, _>("id"),
                    "email": r.get::<String, _>("email"),
                    "country": r.get::<Option<String>, _>("country"),
                    "device_id": r.get::<Option<String>, _>("device_id"),
                    "signed_up_at": r.get::<i64, _>("created_at"),
                    "subscription_status": r.get::<Option<String>, _>("subscription_status"),
                    "requests": r.get::<i64, _>("requests"),
                    "tokens": r.get::<i64, _>("tokens"),
                    "cost_usd": ticks_to_usd(r.get::<i64, _>("cost_ticks")),
                    "last_day": r.get::<Option<String>, _>("last_day"),
                })
            })
            .collect())
    }

    /// Fleet Pro usage per UTC day, newest first — the series that would have
    /// answered "when did the lifetime cap get maxed out?".
    pub async fn pro_usage_by_day(&self, days: i64) -> Result<Vec<Value>, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT day,
                    SUM(requests)   AS requests,
                    SUM(tokens)     AS tokens,
                    SUM(cost_ticks) AS cost_ticks,
                    COUNT(DISTINCT user_id) AS users
               FROM pro_usage
              GROUP BY day
              ORDER BY day DESC
              LIMIT ?",
        )
        .bind(days.clamp(1, 365))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                json!({
                    "day": r.get::<String, _>("day"),
                    "requests": r.get::<i64, _>("requests"),
                    "tokens": r.get::<i64, _>("tokens"),
                    "cost_usd": ticks_to_usd(r.get::<i64, _>("cost_ticks")),
                    "users": r.get::<i64, _>("users"),
                })
            })
            .collect())
    }

    /// Recent rejected registrations. Recorded since day one, surfaced nowhere
    /// until now — account farming leaves its trail here.
    pub async fn recent_rejected_registrations(
        &self,
        hours: i64,
        limit: i64,
    ) -> Result<Vec<Value>, sqlx::Error> {
        let since = now_ms() - hours.clamp(1, 24 * 365) * 3_600_000;
        let rows = sqlx::query(
            "SELECT email, install_id, version, country, reason, ip, ua, created_at
               FROM rejected_registrations
              WHERE created_at >= ?
              ORDER BY created_at DESC
              LIMIT ?",
        )
        .bind(since)
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                json!({
                    "email": r.get::<Option<String>, _>("email"),
                    "install_id": r.get::<Option<String>, _>("install_id"),
                    "version": r.get::<Option<String>, _>("version"),
                    "country": r.get::<Option<String>, _>("country"),
                    "reason": r.get::<String, _>("reason"),
                    "ip": r.get::<Option<String>, _>("ip"),
                    "ua": r.get::<Option<String>, _>("ua"),
                    "at": r.get::<i64, _>("created_at"),
                })
            })
            .collect())
    }

    /// Signups grouped by IP in a window — the shape account farming makes.
    /// Counts accepted registrations; the rejected ones are listed separately.
    pub async fn signups_by_ip(&self, hours: i64, limit: i64) -> Result<Vec<Value>, sqlx::Error> {
        let since = now_ms() - hours.clamp(1, 24 * 365) * 3_600_000;
        let rows = sqlx::query(
            "SELECT ip, COUNT(*) AS n, MIN(created_at) AS first_at, MAX(created_at) AS last_at
               FROM users
              WHERE ip IS NOT NULL AND ip != '' AND created_at >= ?
              GROUP BY ip
              ORDER BY n DESC
              LIMIT ?",
        )
        .bind(since)
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                json!({
                    "ip": r.get::<String, _>("ip"),
                    "signups": r.get::<i64, _>("n"),
                    "first_at": r.get::<i64, _>("first_at"),
                    "last_at": r.get::<i64, _>("last_at"),
                })
            })
            .collect())
    }

    /// Operator reset of free-credit counters for one account (and every other
    /// account sharing its `device_id` — same cohort `pro_user_total_usage` uses).
    /// Returns `(rows_deleted, free_requests_were, free_tokens_were)` so the
    /// admin response can show what was wiped. Distroless prod has no shell;
    /// this is the only supported way to re-open free Pro for a burned install.
    pub async fn reset_pro_usage_for_user(
        &self,
        user_id: i64,
    ) -> Result<(u64, i64, i64), sqlx::Error> {
        let (req_were, tok_were) = self.pro_user_total_usage(user_id).await?;
        let result = sqlx::query(
            "DELETE FROM pro_usage WHERE user_id IN (
                    SELECT u2.id FROM users u1
                      JOIN users u2 ON u2.device_id = u1.device_id
                     WHERE u1.id = ? AND u1.device_id IS NOT NULL AND u1.device_id != ''
                    UNION SELECT ?)",
        )
        .bind(user_id)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok((result.rows_affected(), req_were, tok_were))
    }

    /// Add real spend (xAI ticks) to today's counter. Cost is only ever added —
    /// unlike the pre-charged token estimate there is nothing to reconcile,
    /// because the figure arrives after the call with the bill already settled.
    pub async fn pro_add_cost(&self, user_id: i64, ticks: i64) -> Result<(), sqlx::Error> {
        if ticks <= 0 {
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO pro_usage (user_id, day, requests, tokens, cost_ticks)
             VALUES (?, strftime('%Y-%m-%d','now'), 0, 0, ?)
             ON CONFLICT(user_id, day) DO UPDATE SET cost_ticks = cost_ticks + excluded.cost_ticks",
        )
        .bind(user_id)
        .bind(ticks)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Signed cost adjustment, flooring the row at zero — the dollar twin of
    /// `pro_settle_tokens`. Chat pre-charges an estimate before forwarding so a
    /// huge prompt cannot outrun the cap, then settles the difference once xAI
    /// reports what it really charged.
    pub async fn pro_settle_cost(&self, user_id: i64, delta: i64) -> Result<(), sqlx::Error> {
        if delta == 0 {
            return Ok(());
        }
        if delta > 0 {
            return self.pro_add_cost(user_id, delta).await;
        }
        sqlx::query(
            "UPDATE pro_usage SET cost_ticks = MAX(0, cost_ticks + ?)
              WHERE user_id = ? AND day = strftime('%Y-%m-%d','now')",
        )
        .bind(delta)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Lifetime spend in ticks across ALL users — the dollar breaker's input.
    pub async fn pro_total_cost(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar::<_, i64>("SELECT COALESCE(SUM(cost_ticks), 0) FROM pro_usage")
            .fetch_one(&self.pool)
            .await
    }

    /// Today's spend in ticks across ALL users.
    pub async fn pro_global_cost_today(&self) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(SUM(cost_ticks), 0) FROM pro_usage
              WHERE day = strftime('%Y-%m-%d','now')",
        )
        .fetch_one(&self.pool)
        .await
    }

    /// Spend in ticks for one user over the last `days` UTC days — the paid
    /// rolling window, in dollars.
    pub async fn pro_user_cost_days(&self, user_id: i64, days: i64) -> Result<i64, sqlx::Error> {
        let days = days.max(1);
        let sql = format!(
            "SELECT COALESCE(SUM(cost_ticks), 0)
               FROM pro_usage
              WHERE user_id = ?
                AND day >= strftime('%Y-%m-%d', 'now', '-{days} days')"
        );
        sqlx::query_scalar::<_, i64>(&sql)
            .bind(user_id)
            .fetch_one(&self.pool)
            .await
    }

    /// Today's spend in ticks for one user.
    pub async fn pro_user_cost_today(&self, user_id: i64) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(SUM(cost_ticks), 0) FROM pro_usage
              WHERE user_id = ? AND day = strftime('%Y-%m-%d','now')",
        )
        .bind(user_id)
        .fetch_one(&self.pool)
        .await
    }

    /// Lifetime spend in ticks for one user's device cohort — the same set
    /// `pro_user_total_usage` sums, so free credit in dollars matches free
    /// credit in tokens on who it covers.
    pub async fn pro_user_total_cost(&self, user_id: i64) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar::<_, i64>(
            "SELECT COALESCE(SUM(cost_ticks), 0)
               FROM pro_usage
              WHERE user_id IN (
                    SELECT u2.id FROM users u1
                      JOIN users u2 ON u2.device_id = u1.device_id
                     WHERE u1.id = ? AND u1.device_id IS NOT NULL AND u1.device_id != ''
                    UNION SELECT ?)",
        )
        .bind(user_id)
        .bind(user_id)
        .fetch_one(&self.pool)
        .await
    }

    /// Apply a signed token adjustment to today's counter, flooring the row at
    /// zero. This is how a pre-charged estimate is reconciled: the relay bills
    /// an upfront guess before forwarding (so a huge prompt cannot slip past the
    /// caps on its first call), then settles `real - estimate`, which is
    /// negative whenever the guess was high. A negative delta on a day with no
    /// row is a no-op — there is nothing to give back.
    pub async fn pro_settle_tokens(&self, user_id: i64, delta: i64) -> Result<(), sqlx::Error> {
        if delta == 0 {
            return Ok(());
        }
        if delta > 0 {
            return self.pro_add_tokens(user_id, delta).await;
        }
        sqlx::query(
            "UPDATE pro_usage SET tokens = MAX(0, tokens + ?)
              WHERE user_id = ? AND day = strftime('%Y-%m-%d','now')",
        )
        .bind(delta)
        .bind(user_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Add completion tokens to today's counter. Called after a relay response
    /// completes (fire-and-forget), so a missing/garbled usage block just leaves
    /// the token count low rather than failing the user's request.
    pub async fn pro_add_tokens(&self, user_id: i64, tokens: i64) -> Result<(), sqlx::Error> {
        if tokens <= 0 {
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO pro_usage (user_id, day, requests, tokens)
             VALUES (?, strftime('%Y-%m-%d','now'), 0, ?)
             ON CONFLICT(user_id, day) DO UPDATE SET tokens = tokens + excluded.tokens",
        )
        .bind(user_id)
        .bind(tokens)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// One-shot dump of the Pro tables so `dottie-pro` can import them before
    /// the nginx flip. Identity stays here; we only ship the rows the relay
    /// needs to keep spend caps and Stripe state continuous.
    pub async fn export_pro(&self) -> Result<Value, sqlx::Error> {
        let table = |sql: &'static str| async move {
            let raw: String = sqlx::query_scalar(sql).fetch_one(&self.pool).await?;
            Ok::<Value, sqlx::Error>(serde_json::from_str(&raw).unwrap_or(json!([])))
        };
        Ok(json!({
            "users": table(
                "SELECT COALESCE(json_group_array(json_object(
                    'id', id, 'email', email, 'device_id', device_id,
                    'subscription_stripe_id', subscription_stripe_id,
                    'subscription_status', subscription_status,
                    'subscription_expires', subscription_expires
                 )), '[]') FROM users
                 WHERE COALESCE(subscription_stripe_id,'') != ''
                    OR id IN (SELECT user_id FROM pro_usage
                              UNION SELECT user_id FROM pro_calls
                              UNION SELECT user_id FROM pro_refund_requests)",
            ).await?,
            "pro_usage": table(
                "SELECT COALESCE(json_group_array(json_object(
                    'user_id', user_id, 'day', day, 'requests', requests,
                    'tokens', tokens, 'cost_ticks', cost_ticks
                 )), '[]') FROM pro_usage",
            ).await?,
            "pro_calls": table(
                "SELECT COALESCE(json_group_array(json_object(
                    'id', id, 'user_id', user_id, 'email', email, 'route', route,
                    'model', model, 'outcome', outcome, 'reason', reason,
                    'status', status, 'prompt_tokens', prompt_tokens,
                    'completion_tokens', completion_tokens, 'tokens', tokens,
                    'estimated', estimated, 'seconds', seconds, 'paid', paid,
                    'ip', ip, 'cost_ticks', cost_ticks,
                    'cost_estimated', cost_estimated, 'created_at', created_at
                 )), '[]') FROM pro_calls",
            ).await?,
            "pro_refund_requests": table(
                "SELECT COALESCE(json_group_array(json_object(
                    'id', id, 'user_id', user_id, 'email', email,
                    'customer', customer, 'subscription_id', subscription_id,
                    'status', status, 'requests_today', requests_today,
                    'tokens_today', tokens_today, 'total_requests', total_requests,
                    'total_tokens', total_tokens, 'note', note,
                    'created_at', created_at, 'resolved_at', resolved_at
                 )), '[]') FROM pro_refund_requests",
            ).await?,
            "stripe_webhook_events": table(
                "SELECT COALESCE(json_group_array(json_object(
                    'event_id', event_id, 'event_type', event_type,
                    'created_at', created_at
                 )), '[]') FROM stripe_webhook_events",
            ).await?,
        }))
    }

    // ---- later phases -----------------------------------------------------
    // TODO(A5): port the report/read functions (getStats, getRetention,
    //   getDailyActive, getErrorBreakdown, getErrorsByVersion,
    //   getActivationFunnel, getFunnelReport, getCrashReport,
    //   getNewErrorSignatures, getDiagnosticsList, getDiagnostic).
    // TODO(later): maybeScrubExcludedOnBoot.
}

// ---- free helpers ---------------------------------------------------------

/// Unix seconds — JWT `iat`/`exp` are in seconds; the rest of this file is ms.
fn now_secs() -> i64 {
    now_ms() / 1000
}

/// Unix milliseconds, matching JS `Date.now()`.
fn now_ms() -> i64 {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    // u128 ms -> i64; the value won't overflow until year 292 million.
    i64::try_from(dur.as_millis()).unwrap_or(i64::MAX)
}

/// 32 cryptographically-random bytes, hex-encoded lowercase (64 chars) —
/// matches node `crypto.randomBytes(32).toString('hex')`. Reads from
/// /dev/urandom via std (no `rand` crate), handling short reads. Fails CLOSED:
/// like node's randomBytes, it errors rather than minting a weak token if the
/// OS RNG is unavailable — a guessable bearer token would be a security hole.
fn random_token() -> Result<String, sqlx::Error> {
    let mut buf = [0u8; 32];
    read_urandom(&mut buf).map_err(|e| sqlx::Error::Io(std::io::Error::other(e)))?;
    let mut s = String::with_capacity(64);
    for b in buf {
        s.push_str(&format!("{b:02x}"));
    }
    Ok(s)
}

/// Fill `buf` completely from /dev/urandom, looping over short reads. Returns
/// Err if the device can't be opened or can't supply enough bytes — the caller
/// propagates it (no weak fallback).
fn read_urandom(buf: &mut [u8]) -> Result<(), String> {
    let mut f =
        std::fs::File::open("/dev/urandom").map_err(|e| format!("open /dev/urandom: {e}"))?;
    let mut filled = 0;
    while filled < buf.len() {
        match f.read(&mut buf[filled..]) {
            Ok(0) => return Err("unexpected EOF reading /dev/urandom".to_string()),
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("read /dev/urandom: {e}")),
        }
    }
    Ok(())
}

/// Clamp an optional string to `max` chars (by char, mirroring JS `.slice`),
/// mapping `None` to SQL NULL.
fn clamp_opt(v: &Option<String>, max: usize) -> Option<String> {
    v.as_ref().map(|s| s.chars().take(max).collect())
}

/// Stamp the server-computed experiment arm onto an event's props, so every
/// stored event carries `variant`. The server value wins over a client one: the
/// arm is a pure function of the install id (see `variant`). Array props are
/// left alone (no key to set).
fn with_variant(props: &Option<Value>, install_id: &str) -> Option<Value> {
    let Some(arm) = crate::variant::variant_for(install_id) else {
        return props.clone();
    };
    match props {
        None => Some(json!({ "variant": arm })),
        Some(Value::Object(m)) => {
            let mut m = m.clone();
            m.insert("variant".into(), Value::String(arm.into()));
            Some(Value::Object(m))
        }
        Some(other) => Some(other.clone()),
    }
}

/// Serialize optional JSON to a TEXT value, or NULL when absent — mirrors the
/// JS `v != null ? JSON.stringify(v) : null`.
fn json_or_null(v: &Option<Value>) -> Option<String> {
    v.as_ref().and_then(|value| serde_json::to_string(value).ok())
}

/// Read a props field as a display string for issue fingerprinting: a JSON
/// string as-is, a JSON number stringified, everything else (absent/null/object)
/// → None. Mirrors how the error reports treat file/function/line (string OR
/// number in props).
fn prop_str(props: &Option<Value>, key: &str) -> Option<String> {
    match props.as_ref().and_then(|p| p.get(key)) {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    }
}

/// Compare two client versions numerically, part by part. Handles CalVer
/// (`2026.8.5.1`) and the pre-2026 SemVer tail (`9.24`) in one rule: split on
/// '.', compare each part as a number, missing parts count as 0 — so
/// `9.24 < 2026.8.1.7 < 2026.8.5 < 2026.8.5.1`. Non-numeric parts count as 0.
pub fn version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    let part = |s: &str, i: usize| -> i64 {
        s.split('.').nth(i).and_then(|p| p.parse().ok()).unwrap_or(0)
    };
    let len = a.split('.').count().max(b.split('.').count());
    (0..len)
        .map(|i| part(a, i).cmp(&part(b, i)))
        .find(|o| o.is_ne())
        .unwrap_or(std::cmp::Ordering::Equal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paid_row(status: &str, expires: Option<i64>) -> UserRow {
        UserRow {
            id: 1,
            install_id: "i".into(),
            email: "a@b.c".into(),
            subscription_stripe_id: Some("cus_x".into()),
            subscription_status: Some(status.into()),
            subscription_expires: expires,
        }
    }

    #[test]
    fn is_paid_active_without_expiry() {
        assert!(paid_row("active", None).is_paid());
        assert!(paid_row("trialing", None).is_paid());
        assert!(!paid_row("canceled", None).is_paid());
        assert!(!paid_row("active", Some(1)).is_paid(), "unix epoch expiry is past");
        assert!(paid_row("active", Some(4_000_000_000)).is_paid());
    }

    fn cfg() -> (HashSet<String>, HashSet<String>) {
        // Mirrors the JS test env: DOTTIE_EXCLUDED_INSTALL_IDS=DEV-IID-123,
        // DOTTIE_EXCLUDED_EMAILS="Dev@Internal.Test" (injected pre-lowercased,
        // exactly as main.rs does when parsing env).
        let ids: HashSet<String> = ["DEV-IID-123".to_string()].into_iter().collect();
        let emails: HashSet<String> = ["dev@internal.test".to_string()].into_iter().collect();
        (ids, emails)
    }

    async fn db_from(pool: SqlitePool) -> Db {
        let (ids, emails) = cfg();
        let db = Db::from_pool(pool, ids, emails);
        // #[sqlx::test] applies migrations/ but not ensure_schema, so run it
        // here to create the partial unique indexes (needed for event_id dedupe).
        db.ensure_schema().await.unwrap();
        db
    }

    fn reg(name: &str, email: &str, install_id: &str) -> RegisterUser {
        RegisterUser {
            name: name.to_string(),
            email: email.to_string(),
            install_id: install_id.to_string(),
            version: Some("1".to_string()),
            os_version: None,
            country: None,
            iid: None,
            device_id: None,
            ip: None,
        }
    }

    async fn count(pool: &SqlitePool, sql: &str) -> i64 {
        let row: (i64,) = sqlx::query_as(sql).fetch_one(pool).await.unwrap();
        row.0
    }

    // 1) excluded email: registerUser hands back a token but persists NO row.
    #[sqlx::test]
    async fn excluded_email_token_but_no_row(pool: SqlitePool) {
        let db = db_from(pool).await;
        let token = db
            .register_user(&reg("Dev", "dev@internal.test", "anything"))
            .await
            .unwrap();
        assert_eq!(token.len(), 64);
        assert!(db.get_user_by_token(&token).await.unwrap().is_none());
    }

    // 2) excluded install_id: persists NO row even with a fresh email.
    #[sqlx::test]
    async fn excluded_install_id_no_row(pool: SqlitePool) {
        let db = db_from(pool).await;
        let token = db
            .register_user(&reg("Dev2", "someone-else@example.com", "DEV-IID-123"))
            .await
            .unwrap();
        assert!(db.get_user_by_token(&token).await.unwrap().is_none());
    }

    // 3) non-excluded user persists and is retrievable by token.
    #[sqlx::test]
    async fn real_user_persists(pool: SqlitePool) {
        let db = db_from(pool).await;
        let token = db
            .register_user(&reg("Real", "real@example.com", "real-1"))
            .await
            .unwrap();
        let row = db.get_user_by_token(&token).await.unwrap();
        assert_eq!(row.unwrap().email, "real@example.com");
    }

    // 4) smoke-* install_id excluded by pattern, no env entry needed.
    #[sqlx::test]
    async fn smoke_install_excluded(pool: SqlitePool) {
        let db = db_from(pool).await;
        let token = db
            .register_user(&reg(
                "smoke.test",
                "deploysmoke+999@test.dottie.ai",
                "smoke-test-999",
            ))
            .await
            .unwrap();
        assert_eq!(token.len(), 64);
        assert!(db.get_user_by_token(&token).await.unwrap().is_none());
    }

    // 5) @test.dottie.ai email excluded by pattern even with a non-smoke iid.
    #[sqlx::test]
    async fn test_dottie_email_excluded(pool: SqlitePool) {
        let db = db_from(pool).await;
        let token = db
            .register_user(&reg("Bot", "ci+123@test.dottie.ai", "ci-runner-abc"))
            .await
            .unwrap();
        assert!(db.get_user_by_token(&token).await.unwrap().is_none());
    }

    // 6) real UUID install + ordinary email is NOT caught by the smoke pattern.
    #[sqlx::test]
    async fn real_uuid_not_overmatched(pool: SqlitePool) {
        let db = db_from(pool).await;
        let token = db
            .register_user(&reg(
                "Realer",
                "realer@gmail.com",
                "A1B2C3D4-1111-2222-3333-444455556666",
            ))
            .await
            .unwrap();
        let row = db.get_user_by_token(&token).await.unwrap();
        assert_eq!(row.unwrap().email, "realer@gmail.com");
    }

    // 7) excluded install_id dropped from pings/events; real ones count.
    //    Report fns aren't ported yet, so assert via direct COUNT queries
    //    (distinct engaged installs = chat-sent / voice.activated).
    #[sqlx::test]
    async fn excluded_install_dropped_from_pings_events(pool: SqlitePool) {
        let db = db_from(pool.clone()).await;
        db.record_ping(&Ping {
            install_id: "DEV-IID-123".into(),
            version: Some("1".into()),
        })
        .await
        .unwrap();
        db.record_event(&Event {
            install_id: "DEV-IID-123".into(),
            name: "chat-sent".into(),
            version: Some("1".into()),
            ..Default::default()
        })
        .await
        .unwrap();
        db.record_ping(&Ping {
            install_id: "real-ping-1".into(),
            version: Some("1".into()),
        })
        .await
        .unwrap();
        db.record_event(&Event {
            install_id: "real-ping-1".into(),
            name: "chat-sent".into(),
            version: Some("1".into()),
            ..Default::default()
        })
        .await
        .unwrap();

        // Only real-ping-1 made it into either table.
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM pings").await, 1);
        let engaged = count(
            &pool,
            "SELECT COUNT(DISTINCT install_id) FROM events WHERE name IN ('chat-sent','voice.activated')",
        )
        .await;
        assert_eq!(engaged, 1, "only the non-excluded install_id is engaged");
        let dev_pings = count(
            &pool,
            "SELECT COUNT(*) FROM pings WHERE install_id = 'DEV-IID-123'",
        )
        .await;
        assert_eq!(dev_pings, 0);
    }

    // BONUS A) event_id dedupe: a retried event (same event_id) no-ops.
    #[sqlx::test]
    async fn event_id_dedupe_idempotent(pool: SqlitePool) {
        let db = db_from(pool.clone()).await;
        let mk = || Event {
            install_id: "real-1".into(),
            name: "chat-sent".into(),
            event_id: Some("evt-abc-123".into()),
            ..Default::default()
        };
        db.record_event(&mk()).await.unwrap();
        db.record_event(&mk()).await.unwrap(); // retry
        db.record_event(&mk()).await.unwrap(); // retry again
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM events").await, 1);

        // An event WITHOUT an event_id always inserts (no dedupe).
        let plain = Event {
            install_id: "real-1".into(),
            name: "chat-sent".into(),
            ..Default::default()
        };
        db.record_event(&plain).await.unwrap();
        db.record_event(&plain).await.unwrap();
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM events").await, 3);
    }

    // PHASE 1 A) error events roll into issues; dedupe doesn't bump count;
    //   resolve then recur flips to 'regressed'.
    #[sqlx::test]
    async fn issue_lifecycle(pool: SqlitePool) {
        use serde_json::json;
        let db = db_from(pool.clone()).await;
        let err = |eid: Option<&str>| Event {
            install_id: "real-1".into(),
            name: "error.thrown".into(),
            props: Some(json!({ "file": "a.rs", "function": "f", "line": 10 })),
            event_id: eid.map(str::to_string),
            ..Default::default()
        };
        // First sighting → one active issue, count 1.
        db.record_event(&err(None)).await.unwrap();
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM issues").await, 1);
        assert_eq!(count(&pool, "SELECT count FROM issues").await, 1);
        // A deduped retry (same event_id, fired twice) bumps count only once.
        db.record_event(&err(Some("evt-1"))).await.unwrap();
        db.record_event(&err(Some("evt-1"))).await.unwrap(); // ignored retry
        assert_eq!(count(&pool, "SELECT count FROM issues").await, 2);
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM issues WHERE status='active'").await,
            1
        );
        // Resolve it, then the same signature recurs → regressed, resolved_at cleared.
        let id = count(&pool, "SELECT id FROM issues").await;
        db.set_issue_status(id, "resolved", None).await.unwrap();
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM issues WHERE status='resolved' AND resolved_at IS NOT NULL").await,
            1
        );
        db.record_event(&err(None)).await.unwrap();
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM issues WHERE status='regressed' AND resolved_at IS NULL").await,
            1
        );
        // last_version tracks the newest versioned sighting and survives a
        // version-less one (COALESCE keeps the old value).
        let mut versioned = err(None);
        versioned.version = Some("2026.7.18".into());
        db.record_event(&versioned).await.unwrap();
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM issues WHERE last_version='2026.7.18'").await,
            1
        );
        db.record_event(&err(None)).await.unwrap();
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM issues WHERE last_version='2026.7.18'").await,
            1
        );
    }

    // C2) resolving with a fix version stops OLD builds from regressing the
    //   issue; a sighting from the fix build (or newer) still does.
    #[sqlx::test]
    async fn resolved_in_version_gates_regression(pool: SqlitePool) {
        use serde_json::json;
        let db = db_from(pool.clone()).await;
        let err = |v: &str| Event {
            install_id: "real-1".into(),
            name: "error.thrown".into(),
            props: Some(json!({ "file": "a.rs", "function": "f", "line": 10 })),
            version: Some(v.into()),
            ..Default::default()
        };
        db.record_event(&err("2026.8.1.7")).await.unwrap();
        let id = count(&pool, "SELECT id FROM issues").await;
        db.set_issue_status(id, "resolved", Some("2026.8.5")).await.unwrap();
        let status = |pool: SqlitePool| async move {
            sqlx::query_scalar::<_, String>("SELECT status FROM issues")
                .fetch_one(&pool)
                .await
                .unwrap()
        };

        // Stale build in the field: counts, but stays resolved.
        db.record_event(&err("2026.8.1.7")).await.unwrap();
        assert_eq!(status(pool.clone()).await, "resolved");
        assert_eq!(count(&pool, "SELECT count FROM issues").await, 2);
        // Version-less sighting can't be proven stale either → still resolved.
        db.record_event(&Event {
            version: None,
            ..err("ignored")
        })
        .await
        .unwrap();
        assert_eq!(status(pool.clone()).await, "resolved");

        // The fix build reports it → genuine regression.
        db.record_event(&err("2026.8.5.1")).await.unwrap();
        assert_eq!(status(pool.clone()).await, "regressed");
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM issues WHERE resolved_at IS NULL").await,
            1
        );
    }

    #[test]
    fn version_cmp_orders_calver_and_legacy_semver() {
        use std::cmp::Ordering::*;
        assert_eq!(version_cmp("2026.8.5", "2026.8.1.7"), Greater);
        assert_eq!(version_cmp("2026.8.1.7", "9.24"), Greater);
        assert_eq!(version_cmp("2026.8.5", "2026.8.5"), Equal);
        assert_eq!(version_cmp("2026.8.5.1", "2026.8.5"), Greater);
        assert_eq!(version_cmp("2026.8.5", "2026.8.5.1"), Less);
        // Missing/garbage parts count as 0.
        assert_eq!(version_cmp("2026.8", "2026.8.0"), Equal);
        assert_eq!(version_cmp("2026.8.beta", "2026.8.0"), Equal);
    }

    // PHASE 1 B) register with an iid maps the install for attribution; excluded
    //   installs never map.
    #[sqlx::test]
    async fn iid_mapping(pool: SqlitePool) {
        let db = db_from(pool.clone()).await;
        let mut r = reg("Real", "real@example.com", "iid-install-1");
        r.iid = Some("landing-abc".into());
        db.register_user(&r).await.unwrap();
        assert_eq!(
            count(&pool, "SELECT COUNT(*) FROM install_landing_map WHERE install_id='iid-install-1' AND iid='landing-abc'").await,
            1
        );
        // Excluded (smoke) install with an iid does NOT map.
        let mut s = reg("smoke", "x@test.dottie.ai", "smoke-xyz");
        s.iid = Some("landing-def".into());
        db.register_user(&s).await.unwrap();
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM install_landing_map").await, 1);
    }

    // BONUS B) token preserved on re-register (never rotated).
    #[sqlx::test]
    async fn token_preserved_on_reregister(pool: SqlitePool) {
        let db = db_from(pool.clone()).await;
        let t1 = db
            .register_user(&reg("Real", "real@example.com", "iid-1"))
            .await
            .unwrap();
        // Re-register same email, different name/install_id/version.
        let mut again = reg("Real Renamed", "real@example.com", "iid-2");
        again.version = Some("2".into());
        let t2 = db.register_user(&again).await.unwrap();
        assert_eq!(t1, t2, "token must not rotate on re-register");
        // Still exactly one row, with the updated install_id.
        assert_eq!(count(&pool, "SELECT COUNT(*) FROM users").await, 1);
        let row = db.get_user_by_token(&t1).await.unwrap().unwrap();
        assert_eq!(row.install_id, "iid-2");
    }
}
