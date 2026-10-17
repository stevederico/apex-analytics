//! The telemetry REPORT/READ layer — a faithful sqlx async port of the report
//! functions in `backend/db.js` (getStats, getRetention, getDailyActive,
//! getErrorBreakdown, getErrorsByVersion, getActivationFunnel, getFunnelReport,
//! getCrashReport, getNewErrorSignatures, getDiagnosticsList, getDiagnostic).
//!
//! Each function returns a `serde_json::Value` whose shape MATCHES the JS
//! `JSON.stringify` output byte-for-byte (so the A6 cross-impl diff passes). The
//! SQL is copied verbatim from db.js (json_extract paths, GROUP BY, HAVING NOT
//! EXISTS, the crash top-frame substr/instr expression, etc.).
//!
//! PARITY HAZARDS handled here:
//!   - FLOAT FORMATTING: stickiness / activation_rate / retention rates use JS
//!     `Math.round(x*100)/100`. JS prints an integral result as an integer
//!     ("100", not "100.0"); serde_json prints an f64 as "100.0". `js_round2`
//!     returns a `Value` that is `Number(i64)` when the rounded value is integral
//!     and `Number(f64)` otherwise, replicating JS number formatting. The
//!     denominator-zero case is JSON `null`.
//!   - TIMESTAMPS: epoch-ms `i64` → the exact string `new Date(ms).toISOString()`
//!     produces (`YYYY-MM-DDTHH:MM:SS.mmmZ`), hand-formatted from the epoch (no
//!     chrono dependency).
//!   - NULLABLE json_extract: read as `Option<...>`; the loosely-typed
//!     file/function/line/signal columns (string OR number in props) are read via
//!     `json_value` which preserves the SQLite native type as a JSON value,
//!     exactly like JS passing `r.col` straight into the response object.

use std::collections::HashSet;

use serde_json::{json, Map, Value};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, TypeInfo, ValueRef};

use crate::db::Db;
use crate::validate::{js_number, json_num, parse_num_query};

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// f64 millis-per-hour for computing `since` from a (possibly fractional) hours
/// window, matching JS `now - h * 3600 * 1000` arithmetic.
const HOUR_MS_F: f64 = 60.0 * 60.0 * 1000.0;
const HOUR_MS: i64 = 60 * 60 * 1000;

/// "Engaged" event names — installs that used the product for its purpose.
/// Desktop no longer emits `chat-sent` for Dottie Pro (server `pro_calls` /
/// `today.dau` on dottie-pro owns that). These names still count local/BYOK
/// opt-in client events and any legacy rows already stored.
const ENGAGED_EVENT_NAMES: [&str; 2] = ["chat-sent", "voice.activated"];

/// Every "this install was active at `created_at`" signal, as a derived table
/// with `(install_id, created_at)`. Desktop 2026.10.10+ stopped calling
/// `/api/ping` and sends one daily `app-launched` event instead, and Pro relay
/// calls prove activity too, so DAU/WAU/MAU, retention and lifecycle read this
/// instead of `pings` alone. A Pro user maps to `users.install_id`, or
/// `user:<id>` until the install is linked. `pro_usage` is per-UTC-day only, so
/// its rows stand at that day's midnight; it covers history before `pro_calls`.
const ACTIVE_INSTALLS: &str = "(SELECT install_id, created_at FROM pings \
     UNION ALL SELECT install_id, created_at FROM events WHERE name = 'app-launched' \
     UNION ALL SELECT COALESCE(NULLIF(u.install_id, ''), 'user:' || c.user_id), c.created_at \
       FROM pro_calls c LEFT JOIN users u ON u.id = c.user_id \
     UNION ALL SELECT COALESCE(NULLIF(u.install_id, ''), 'user:' || pu.user_id), \
       CAST(strftime('%s', pu.day) AS INTEGER) * 1000 \
       FROM pro_usage pu LEFT JOIN users u ON u.id = pu.user_id)";

/// True when any of the ascending `ts` falls in `[lo, hi]`.
fn active_between(ts: &[i64], lo: i64, hi: i64) -> bool {
    let i = ts.partition_point(|&t| t < lo);
    i < ts.len() && ts[i] <= hi
}

/// Operator-noise installs/emails omitted from daily-report person lists only.
/// Totals / DAU are unaffected. Read from `REPORT_EXCLUDE_EMAILS` and
/// `REPORT_EXCLUDE_INSTALLS` (comma-separated); empty excludes nothing. Both
/// sides are stored lowercased so matching is case-insensitive.
#[derive(Clone, Debug, Default)]
pub struct ReportExclude {
    emails: HashSet<String>,
    installs: HashSet<String>,
}

impl ReportExclude {
    /// Build from comma-separated lists; entries are trimmed, blanks dropped.
    pub fn new(emails: &str, installs: &str) -> Self {
        let parse = |raw: &str| -> HashSet<String> {
            raw.split(',')
                .map(|s| s.trim().to_ascii_lowercase())
                .filter(|s| !s.is_empty())
                .collect()
        };
        Self {
            emails: parse(emails),
            installs: parse(installs),
        }
    }

    /// `REPORT_EXCLUDE_EMAILS` / `REPORT_EXCLUDE_INSTALLS`; unset = empty.
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).unwrap_or_default();
        Self::new(
            &var("REPORT_EXCLUDE_EMAILS"),
            &var("REPORT_EXCLUDE_INSTALLS"),
        )
    }

    /// True when either the email or the install id is on the list.
    pub fn matches(&self, email: Option<&str>, install_id: Option<&str>) -> bool {
        email.is_some_and(|e| self.emails.contains(&e.to_ascii_lowercase()))
            || install_id.is_some_and(|i| self.installs.contains(&i.to_ascii_lowercase()))
    }
}

/// The engaged names as a SQL `IN (...)` list literal: `'chat-sent','voice.activated'`.
/// The names are compile-time constants (never user input), so interpolation is safe.
fn engaged_in_list() -> String {
    ENGAGED_EVENT_NAMES
        .iter()
        .map(|s| format!("'{s}'"))
        .collect::<Vec<_>>()
        .join(",")
}

impl Db {
    // ---- getStats ---------------------------------------------------------

    /// Top-line metrics: user count, launch-based DAU/WAU/MAU, engagement-based
    /// counts, stickiness, 24h ping/event totals, top-20 events, and country
    /// breakdowns. Faithful port of db.js `getStats`.
    pub async fn get_stats(&self) -> Result<Value, sqlx::Error> {
        let now = now_ms();
        let day_ago = now - DAY_MS;
        let week_ago = now - 7 * DAY_MS;
        let month_ago = now - 30 * DAY_MS;

        let users = self.scalar_i64("SELECT COUNT(*) AS n FROM users", &[]).await?;
        let dau = self.count_dau(day_ago).await?;
        let wau = self.count_dau(week_ago).await?;
        let mau = self.count_dau(month_ago).await?;
        let dau_engaged = self.count_engaged(day_ago).await?;
        let wau_engaged = self.count_engaged(week_ago).await?;
        let mau_engaged = self.count_engaged(month_ago).await?;
        let pings_24h = self
            .scalar_i64("SELECT COUNT(*) AS n FROM pings WHERE created_at >= ?", &[day_ago])
            .await?;
        let events_24h = self
            .scalar_i64("SELECT COUNT(*) AS n FROM events WHERE created_at >= ?", &[day_ago])
            .await?;

        // top_events_24h (top 20).
        let top_rows = sqlx::query(
            "SELECT name, COUNT(*) AS n FROM events WHERE created_at >= ? GROUP BY name ORDER BY n DESC LIMIT 20",
        )
        .bind(day_ago)
        .fetch_all(self.pool())
        .await?;
        let top: Vec<Value> = top_rows
            .iter()
            .map(|r| {
                json!({
                    "name": r.get::<String, _>("name"),
                    "count": r.get::<i64, _>("n"),
                })
            })
            .collect();

        // countries (all-time) + new_users_24h_by_country.
        let total_countries = self
            .country_breakdown(
                "SELECT COALESCE(country, 'unknown') AS country, COUNT(*) AS n
                   FROM users GROUP BY country ORDER BY n DESC",
                None,
            )
            .await?;
        let new_countries_24h = self
            .country_breakdown(
                "SELECT COALESCE(country, 'unknown') AS country, COUNT(*) AS n
                   FROM users WHERE created_at >= ? GROUP BY country ORDER BY n DESC",
                Some(day_ago),
            )
            .await?;

        // stickiness = round((dau/mau)*100,2) or null when mau=0.
        let stickiness = if mau > 0 {
            js_round2((dau as f64) / (mau as f64))
        } else {
            Value::Null
        };

        Ok(json!({
            "users": users,
            "dau": dau,
            "wau": wau,
            "mau": mau,
            "dau_engaged": dau_engaged,
            "wau_engaged": wau_engaged,
            "mau_engaged": mau_engaged,
            "stickiness": stickiness,
            "pings_24h": pings_24h,
            "events_24h": events_24h,
            "top_events_24h": top,
            "countries": total_countries,
            "new_users_24h_by_country": new_countries_24h,
        }))
    }

    /// Top event names over `[since, until)` (top 20). Range-aware twin of the
    /// fixed `top_events_24h` in getStats, so the Overview list can follow the
    /// header range picker.
    pub async fn get_top_events(&self, since: i64, until: i64) -> Result<Value, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT name, COUNT(*) AS n FROM events WHERE created_at >= ? AND created_at < ?
               GROUP BY name ORDER BY n DESC LIMIT 20",
        )
        .bind(since)
        .bind(until)
        .fetch_all(self.pool())
        .await?;
        let out: Vec<Value> = rows
            .iter()
            .map(|r| json!({ "name": r.get::<String, _>("name"), "count": r.get::<i64, _>("n") }))
            .collect();
        Ok(json!({ "rows": out }))
    }

    /// Distinct active installs (`ACTIVE_INSTALLS`) in `[since, until)` — unique users over
    /// the selected window, the range-scoped companion to Avg/Peak DAU.
    pub async fn get_active_users(&self, since: i64, until: i64) -> Result<Value, sqlx::Error> {
        let n = self
            .scalar_i64(
                &format!(
                    "SELECT COUNT(DISTINCT install_id) AS n FROM {ACTIVE_INSTALLS} AS a \
                     WHERE created_at >= ? AND created_at < ?"
                ),
                &[since, until],
            )
            .await?;
        Ok(json!({ "active_users": n }))
    }

    async fn count_dau(&self, since: i64) -> Result<i64, sqlx::Error> {
        self.scalar_i64(
            &format!(
                "SELECT COUNT(DISTINCT install_id) AS n FROM {ACTIVE_INSTALLS} AS a WHERE created_at >= ?"
            ),
            &[since],
        )
        .await
    }

    async fn count_engaged(&self, since: i64) -> Result<i64, sqlx::Error> {
        let sql = format!(
            "SELECT COUNT(DISTINCT install_id) AS n FROM events WHERE name IN ({}) AND created_at >= ?",
            engaged_in_list()
        );
        self.scalar_i64(&sql, &[since]).await
    }

    async fn country_breakdown(&self, sql: &str, bind: Option<i64>) -> Result<Vec<Value>, sqlx::Error> {
        let mut q = sqlx::query(sql);
        if let Some(b) = bind {
            q = q.bind(b);
        }
        let rows = q.fetch_all(self.pool()).await?;
        Ok(rows
            .iter()
            .map(|r| {
                json!({
                    "country": r.get::<String, _>("country"),
                    "count": r.get::<i64, _>("n"),
                })
            })
            .collect())
    }

    // ---- getRetention -----------------------------------------------------

    /// Weekly cohort retention. A cohort = installs whose FIRST activity (`ACTIVE_INSTALLS`) fell in an
    /// ISO week (Monday 00:00 UTC). Each D1/D7/D30 bucket's rate is null until the
    /// window has fully elapsed for the member. Faithful port of `getRetention`.
    pub async fn get_retention(&self, weeks: f64) -> Result<Value, sqlx::Error> {
        self.get_retention_for_variant(weeks, None).await
    }

    /// `get_retention` limited to one experiment arm (`variant::variant_for`).
    pub async fn get_retention_for_variant(
        &self,
        weeks: f64,
        variant: Option<&str>,
    ) -> Result<Value, sqlx::Error> {
        let now = now_ms();
        // JS: Math.max(1, Math.min(26, Number.isFinite(weeks) ? weeks : 8)). The
        // (possibly fractional) count is then used as a loop/slice bound below.
        let n: f64 = clamp_hours(weeks, 1.0, 26.0, 8.0);

        // first_seen per install.
        let activity = self.activity_by_install().await?;

        // engaged install set.
        let eng_sql = format!(
            "SELECT DISTINCT install_id FROM events WHERE name IN ({})",
            engaged_in_list()
        );
        let eng_rows = sqlx::query(&eng_sql).fetch_all(self.pool()).await?;
        let engaged: std::collections::HashSet<String> = eng_rows
            .iter()
            .map(|r| r.get::<String, _>("install_id"))
            .collect();

        // Bucket each install's first_seen into its ISO-week start (Monday 00:00 UTC).
        // Map week -> Vec<(install_id, first)>. BTreeMap keeps weeks sorted so we
        // can take the most-recent N (sort + reverse + slice in db.js).
        let mut cohorts: std::collections::BTreeMap<String, Vec<(String, i64)>> =
            std::collections::BTreeMap::new();
        for (install_id, ts) in &activity {
            if !crate::variant::matches(variant, install_id) {
                continue;
            }
            let first = ts[0];
            let install_id = install_id.clone();
            let week = iso_week_start(first);
            cohorts.entry(week).or_default().push((install_id, first));
        }

        // [...entries].sort().reverse().slice(0, n): most-recent n weeks, descending.
        // JS Array.prototype.slice truncates a fractional `n` toward zero; `n as
        // usize` does the same (n is clamped to [1,26], so this never overflows).
        let selected: Vec<(String, Vec<(String, i64)>)> = cohorts
            .into_iter()
            .rev()
            .take(n as usize)
            .collect();

        // Inclusive end boundary windows (start..=end after first_seen).
        let buckets = [
            ("d1", DAY_MS, 2 * DAY_MS - 1),
            ("d7", 6 * DAY_MS, 8 * DAY_MS - 1),
            ("d30", 29 * DAY_MS, 31 * DAY_MS - 1),
        ];

        let mut result: Vec<Value> = Vec::with_capacity(selected.len());
        for (week, members) in &selected {
            let size = members.len() as i64;
            let engaged_count = members
                .iter()
                .filter(|(iid, _)| engaged.contains(iid))
                .count() as i64;

            let mut row = Map::new();
            row.insert("week".into(), Value::String(week.clone()));
            row.insert("size".into(), json!(size));
            row.insert("engaged".into(), json!(engaged_count));

            for (key, start, end) in buckets {
                let mut returned = 0i64;
                let mut of = 0i64;
                for (iid, first) in members {
                    // Only count members whose window has fully elapsed.
                    if now < first + end {
                        continue;
                    }
                    of += 1;
                    if active_between(&activity[iid], first + start, first + end) {
                        returned += 1;
                    }
                }
                let rate = if of > 0 {
                    js_round2((returned as f64) / (of as f64))
                } else {
                    Value::Null
                };
                row.insert(
                    key.into(),
                    json!({ "rate": rate, "returned": returned, "of": of }),
                );
            }
            result.push(Value::Object(row));
        }

        // db.js does result.reverse() — the selected list was descending, so the
        // final output is ascending (oldest cohort first).
        result.reverse();

        Ok(json!({ "generated_at": now, "variant": variant, "cohorts": result }))
    }

    /// Retention for cohorts whose ISO-week start falls in `[since, until)`.
    /// Powers the dashboard picker (custom range / all). Capped to the newest 26
    /// weeks — retention buckets every member in Rust, so an unbounded "all" would
    /// scan every cohort. yagni: 26-week cap, widen if a longer view is needed.
    pub async fn get_retention_window(&self, since: i64, until: i64) -> Result<Value, sqlx::Error> {
        self.get_retention_window_for_variant(since, until, None).await
    }

    /// `get_retention_window` limited to one experiment arm.
    pub async fn get_retention_window_for_variant(
        &self,
        since: i64,
        until: i64,
        variant: Option<&str>,
    ) -> Result<Value, sqlx::Error> {
        let now = now_ms();
        let activity = self.activity_by_install().await?;

        let eng_sql = format!(
            "SELECT DISTINCT install_id FROM events WHERE name IN ({})",
            engaged_in_list()
        );
        let eng_rows = sqlx::query(&eng_sql).fetch_all(self.pool()).await?;
        let engaged: std::collections::HashSet<String> = eng_rows
            .iter()
            .map(|r| r.get::<String, _>("install_id"))
            .collect();

        let mut cohorts: std::collections::BTreeMap<String, Vec<(String, i64)>> =
            std::collections::BTreeMap::new();
        for (install_id, ts) in &activity {
            if !crate::variant::matches(variant, install_id) {
                continue;
            }
            let first = ts[0];
            let install_id = install_id.clone();
            let week = iso_week_start(first);
            cohorts.entry(week).or_default().push((install_id, first));
        }

        // Weeks whose Monday-start is inside the window; newest 26 (descending).
        let selected: Vec<(String, Vec<(String, i64)>)> = cohorts
            .into_iter()
            .filter(|(week, _)| {
                parse_ymd_ms(week).is_some_and(|ms| ms >= since && ms < until)
            })
            .rev()
            .take(26)
            .collect();

        let buckets = [
            ("d1", DAY_MS, 2 * DAY_MS - 1),
            ("d7", 6 * DAY_MS, 8 * DAY_MS - 1),
            ("d30", 29 * DAY_MS, 31 * DAY_MS - 1),
        ];

        let mut result: Vec<Value> = Vec::with_capacity(selected.len());
        for (week, members) in &selected {
            let size = members.len() as i64;
            let engaged_count = members
                .iter()
                .filter(|(iid, _)| engaged.contains(iid))
                .count() as i64;

            let mut row = Map::new();
            row.insert("week".into(), Value::String(week.clone()));
            row.insert("size".into(), json!(size));
            row.insert("engaged".into(), json!(engaged_count));

            for (key, start, end) in buckets {
                let mut returned = 0i64;
                let mut of = 0i64;
                for (iid, first) in members {
                    if now < first + end {
                        continue;
                    }
                    of += 1;
                    if active_between(&activity[iid], first + start, first + end) {
                        returned += 1;
                    }
                }
                let rate = if of > 0 {
                    js_round2((returned as f64) / (of as f64))
                } else {
                    Value::Null
                };
                row.insert(
                    key.into(),
                    json!({ "rate": rate, "returned": returned, "of": of }),
                );
            }
            result.push(Value::Object(row));
        }

        // selected is descending; reverse to ascending (oldest first), as getStats.
        result.reverse();
        Ok(json!({ "generated_at": now, "variant": variant, "cohorts": result }))
    }

    // ---- getDailyActive ---------------------------------------------------

    /// Daily active sparkline: one row per UTC calendar day, newest first.
    /// Faithful port of `getDailyActive`.
    pub async fn get_daily_active(&self, days: f64) -> Result<Value, sqlx::Error> {
        // JS: Math.max(1, Math.min(180, Number.isFinite(days) ? days : 30)). The
        // (possibly fractional) count bounds the day loop below.
        let n: f64 = clamp_hours(days, 1.0, 180.0, 30.0);
        let now = now_ms();
        let today_midnight = (now / DAY_MS) * DAY_MS;

        let engaged_sql = format!(
            "SELECT COUNT(DISTINCT install_id) AS n FROM events WHERE name IN ({}) AND created_at >= ? AND created_at < ?",
            engaged_in_list()
        );

        let mut rows: Vec<Value> = Vec::with_capacity(n.ceil() as usize);
        // JS `for (let i = 0; i < n; i++)` with a float `n`: iterate while
        // (i as f64) < n — a fractional n still admits the integer i below it.
        let mut i: i64 = 0;
        while (i as f64) < n {
            let start = today_midnight - i * DAY_MS;
            let end = start + DAY_MS;
            let dau: i64 = sqlx::query_scalar(&format!(
                "SELECT COUNT(DISTINCT install_id) FROM {ACTIVE_INSTALLS} AS a \
                 WHERE created_at >= ? AND created_at < ?"
            ))
            .bind(start)
            .bind(end)
            .fetch_one(self.pool())
            .await?;
            let dau_engaged: i64 = sqlx::query_scalar(&engaged_sql)
                .bind(start)
                .bind(end)
                .fetch_one(self.pool())
                .await?;
            rows.push(json!({
                "date": iso_date(start),
                "dau": dau,
                "dau_engaged": dau_engaged,
            }));
            i += 1;
        }
        Ok(json!({ "days": rows }))
    }

    // ---- getSeries (dashboard time-series) --------------------------------

    /// Generic per-UTC-day volume series for one metric over `[since_ms, until_ms)`.
    /// Ascending (oldest→newest), contiguous, ZERO-FILLED — every UTC day in the
    /// window is present, count 0 when none. (Deliberately opposite to
    /// `get_daily_active`'s newest-first shape; this saves the frontend a reverse.)
    ///
    /// One grouped scan (`GROUP BY created_at/DAY_MS`) instead of the per-day loop:
    /// the day-bucket division uses the SAME boundary as `get_daily_active`'s
    /// `today_midnight = (now/DAY_MS)*DAY_MS`, so the `dau` series aligns row-for-row
    /// with `/api/stats/active`. New endpoint — no db.js counterpart, so no JS parity.
    pub async fn get_series(
        &self,
        metric: &str,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Value, sqlx::Error> {
        let (table, count_expr, filter) = series_source(metric);
        // `filter` starts with " AND ", so the WHERE always has a leading predicate.
        let sql = format!(
            "SELECT created_at / {DAY_MS} AS bucket, {count_expr} AS n \
             FROM {table} WHERE created_at >= ? AND created_at < ?{filter} \
             GROUP BY bucket ORDER BY bucket"
        );
        let rows = sqlx::query(&sql)
            .bind(since_ms)
            .bind(until_ms)
            .fetch_all(self.pool())
            .await?;
        let mut counts: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        for r in &rows {
            counts.insert(r.get::<i64, _>("bucket"), r.get::<i64, _>("n"));
        }
        // since_ms is a UTC midnight; iterate day buckets inclusive of the last day.
        let first = since_ms / DAY_MS;
        let last = (until_ms - 1) / DAY_MS;
        let mut series: Vec<Value> = Vec::with_capacity((last - first + 1).max(0) as usize);
        let mut bucket = first;
        while bucket <= last {
            series.push(json!({
                "date": iso_date(bucket * DAY_MS),
                "count": counts.get(&bucket).copied().unwrap_or(0),
            }));
            bucket += 1;
        }
        Ok(json!({ "metric": metric, "series": series }))
    }

    /// Same metrics as `get_series`, bucketed by HOUR over `[since, until)` for a
    /// rolling short window (the 24h view). Each point's `date` is a full ISO-Z
    /// timestamp so the chart can label it as an hour.
    pub async fn get_series_hourly(
        &self,
        metric: &str,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Value, sqlx::Error> {
        let (table, count_expr, filter) = series_source(metric);
        let sql = format!(
            "SELECT created_at / {HOUR_MS} AS bucket, {count_expr} AS n \
             FROM {table} WHERE created_at >= ? AND created_at < ?{filter} \
             GROUP BY bucket ORDER BY bucket"
        );
        let rows = sqlx::query(&sql)
            .bind(since_ms)
            .bind(until_ms)
            .fetch_all(self.pool())
            .await?;
        let mut counts: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        for r in &rows {
            counts.insert(r.get::<i64, _>("bucket"), r.get::<i64, _>("n"));
        }
        let first = since_ms / HOUR_MS;
        let last = (until_ms - 1) / HOUR_MS;
        let mut series: Vec<Value> = Vec::with_capacity((last - first + 1).max(0) as usize);
        let mut bucket = first;
        while bucket <= last {
            series.push(json!({
                "date": iso_ms(bucket * HOUR_MS),
                "count": counts.get(&bucket).copied().unwrap_or(0),
            }));
            bucket += 1;
        }
        Ok(json!({ "metric": metric, "series": series }))
    }

    // ---- getErrorBreakdown ------------------------------------------------

    /// `error.thrown` hotspots grouped by file/function/line over `hours`
    /// (clamped [1,720]); top 50. Faithful port of `getErrorBreakdown`.
    ///
    /// SPECIAL CASE: unlike the other windowed reports, the `/api/stats/errors`
    /// route passes the RAW query value straight in (NOT pre-parsed via
    /// parseIntQuery). db.js does `const n = Number(hours); const h =
    /// Number.isFinite(n) ? Math.max(1, Math.min(720, n)) : 24;`. So a MISSING
    /// param (`None` → `Number(undefined)` → NaN) → 24; `""` → `Number("")` → 0 →
    /// clamp → 1; `"48.5"` → 48.5 (kept, echoed as a fractional window).
    pub async fn get_error_breakdown(&self, hours: Option<&str>) -> Result<Value, sqlx::Error> {
        // Number(undefined) is NaN; Number(rawString) otherwise.
        let n = match hours {
            None => f64::NAN,
            Some(s) => js_number(s),
        };
        let h: f64 = if n.is_finite() { n.clamp(1.0, 720.0) } else { 24.0 };
        let since = now_ms_f() - h * HOUR_MS_F;
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM events WHERE name = 'error.thrown' AND created_at >= ?",
        )
        .bind(since)
        .fetch_one(self.pool())
        .await?;
        let rows = sqlx::query(
            "SELECT
               json_extract(props, '$.file')     AS file,
               json_extract(props, '$.function') AS function,
               json_extract(props, '$.line')     AS line,
               COUNT(*) AS n
             FROM events
             WHERE name = 'error.thrown'
               AND created_at >= ?
             GROUP BY file, function, line
             ORDER BY n DESC
             LIMIT 50",
        )
        .bind(since)
        .fetch_all(self.pool())
        .await?;
        let top: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "file": json_value(r, "file"),
                    "function": json_value(r, "function"),
                    "line": json_value(r, "line"),
                    "count": r.get::<i64, _>("n"),
                })
            })
            .collect();
        Ok(json!({ "total": total, "window_hours": json_num(h), "top": top }))
    }

    // ---- getErrorsByVersion -----------------------------------------------

    /// All `error.%` events grouped by version + name, with raw count and
    /// distinct-install count, over `hours` (clamped [1,2160]). Port of
    /// `getErrorsByVersion`.
    pub async fn get_errors_by_version(&self, hours: f64) -> Result<Value, sqlx::Error> {
        let h = clamp_hours(hours, 1.0, 2160.0, 168.0);
        let since = now_ms_f() - h * HOUR_MS_F;
        let rows = sqlx::query(
            "SELECT
               COALESCE(version, 'unknown') AS version,
               name,
               COUNT(*)                     AS n,
               COUNT(DISTINCT install_id)   AS installs
             FROM events
             WHERE name LIKE 'error.%' AND created_at >= ?
             GROUP BY version, name
             ORDER BY n DESC",
        )
        .bind(since)
        .fetch_all(self.pool())
        .await?;
        let out: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "version": r.get::<String, _>("version"),
                    "name": r.get::<String, _>("name"),
                    "count": r.get::<i64, _>("n"),
                    "installs": r.get::<i64, _>("installs"),
                })
            })
            .collect();
        Ok(json!({ "window_hours": json_num(h), "rows": out }))
    }

    // ---- getErrorSignatures -----------------------------------------------

    /// Individual failure modes for all `error.%` events — not just event name.
    /// Groups by version + name + code + reason + provider + file/function/line
    /// so `error.realtime_server_error` splits into e.g. llm_error/xai vs
    /// xai_voice_unavailable/dottiepro. `code`/`provider` fall back to
    /// `category`/`component` (gateway_buffered). Top 40 by distinct installs
    /// then count so one-install connection storms do not bury real errors.
    /// `hours` clamped [1,720].
    pub async fn get_error_signatures(&self, hours: f64) -> Result<Value, sqlx::Error> {
        let h = clamp_hours(hours, 1.0, 720.0, 168.0);
        let since = now_ms_f() - h * HOUR_MS_F;
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM events WHERE name LIKE 'error.%' AND created_at >= ?",
        )
        .bind(since)
        .fetch_one(self.pool())
        .await?;
        let rows = sqlx::query(
            "SELECT
               COALESCE(version, 'unknown')                         AS version,
               name,
               COALESCE(json_extract(props, '$.code'), json_extract(props, '$.category'), '') AS code,
               COALESCE(json_extract(props, '$.reason'), '')        AS reason,
               COALESCE(json_extract(props, '$.provider'), json_extract(props, '$.component'), '') AS provider,
               COALESCE(json_extract(props, '$.file'), '')          AS file,
               COALESCE(json_extract(props, '$.function'), '')      AS function,
               COALESCE(CAST(json_extract(props, '$.line') AS TEXT), '') AS line,
               COALESCE(CAST(json_extract(props, '$.signal') AS TEXT), '') AS signal,
               COALESCE(json_extract(props, '$.exception_type'), '')  AS exception_type,
               COALESCE(json_extract(props, '$.source'), '')          AS source,
               CASE
                 WHEN json_extract(props, '$.hang_s') IS NULL THEN ''
                 WHEN CAST(json_extract(props, '$.hang_s') AS REAL) < 5  THEN '0-5'
                 WHEN CAST(json_extract(props, '$.hang_s') AS REAL) < 10 THEN '5-10'
                 WHEN CAST(json_extract(props, '$.hang_s') AS REAL) < 30 THEN '10-30'
                 WHEN CAST(json_extract(props, '$.hang_s') AS REAL) < 60 THEN '30-60'
                 ELSE '60+'
               END                                                  AS hang_s,
               COUNT(*)                                             AS n,
               COUNT(DISTINCT install_id)                           AS installs
             FROM events
             WHERE name LIKE 'error.%' AND created_at >= ?
             GROUP BY version, name, code, reason, provider, file, function, line,
                      signal, exception_type, source, hang_s
             ORDER BY installs DESC, n DESC
             LIMIT 40",
        )
        .bind(since)
        .fetch_all(self.pool())
        .await?;
        let out: Vec<Value> = rows
            .iter()
            .map(|r| {
                let name: String = r.get("name");
                let code: String = r.get("code");
                let reason: String = r.get("reason");
                let provider: String = r.get("provider");
                let file: String = r.get("file");
                let function: String = r.get("function");
                let line: String = r.get("line");
                // Human label for reports (one glance).
                let detail = if !code.is_empty() || !reason.is_empty() {
                    let mut parts = Vec::new();
                    if !code.is_empty() {
                        parts.push(code.clone());
                    }
                    if !reason.is_empty() {
                        parts.push(reason.clone());
                    }
                    if !provider.is_empty() {
                        parts.push(format!("provider={provider}"));
                    }
                    parts.join(" · ")
                } else if !file.is_empty() {
                    format!(
                        "{}:{} {}",
                        file,
                        if line.is_empty() { "?" } else { &line },
                        function
                    )
                } else {
                    // No code/reason/file — fall back to the distinguishing
                    // props so e.g. error.app_hang shows its duration bucket
                    // instead of a useless "(no props)".
                    let extras: Vec<String> = ["hang_s", "signal", "exception_type", "source"]
                        .iter()
                        .filter_map(|k| {
                            let v: String = r.get(*k);
                            (!v.is_empty()).then(|| format!("{k}={v}"))
                        })
                        .collect();
                    if extras.is_empty() {
                        "(no props)".into()
                    } else {
                        extras.join(" · ")
                    }
                };
                json!({
                    "version": r.get::<String, _>("version"),
                    "name": name,
                    "code": if code.is_empty() { Value::Null } else { json!(code) },
                    "reason": if reason.is_empty() { Value::Null } else { json!(reason) },
                    "provider": if provider.is_empty() { Value::Null } else { json!(provider) },
                    "file": if file.is_empty() { Value::Null } else { json!(file) },
                    "function": if function.is_empty() { Value::Null } else { json!(function) },
                    "line": if line.is_empty() { Value::Null } else { json!(line) },
                    "detail": detail,
                    "count": r.get::<i64, _>("n"),
                    "installs": r.get::<i64, _>("installs"),
                })
            })
            .collect();
        Ok(json!({
            "total": total,
            "window_hours": json_num(h),
            "rows": out,
        }))
    }

    // ---- getEventDetails ---------------------------------------------------

    /// Raw recent events for ONE exact event name, props included — the
    /// drill-down the grouped reports can't answer (e.g. the diag payload on
    /// error.conversation_engine_init_failed). Exact-match only (no LIKE) so an
    /// admin typo can't dump broad swaths of the events table; newest first.
    /// `hours` clamped [1,2160] default 720; `limit` clamped [1,200] default 50.
    /// Port of `getEventDetails`.
    pub async fn get_event_details(
        &self,
        name: &str,
        hours: f64,
        limit: f64,
    ) -> Result<Value, sqlx::Error> {
        let h = clamp_hours(hours, 1.0, 2160.0, 720.0);
        let lim: i64 = if limit.is_finite() {
            (limit.floor() as i64).clamp(1, 200)
        } else {
            50
        };
        let since = now_ms_f() - h * HOUR_MS_F;
        let rows = sqlx::query(
            "SELECT install_id, version, props,
                    datetime(created_at/1000, 'unixepoch') AS at
             FROM events
             WHERE name = ? AND created_at >= ?
             ORDER BY created_at DESC
             LIMIT ?",
        )
        .bind(name)
        .bind(since)
        .bind(lim)
        .fetch_all(self.pool())
        .await?;
        let out: Vec<Value> = rows
            .iter()
            .map(|r| {
                let version: Option<String> = r.get("version");
                let props: Option<String> = r.get("props");
                json!({
                    "install_id": r.get::<String, _>("install_id"),
                    "version": version,
                    "at": r.get::<String, _>("at"),
                    "props": parse_json_or_null(props),
                })
            })
            .collect();
        Ok(json!({ "name": name, "window_hours": json_num(h), "rows": out }))
    }

    // ---- getActivationFunnel ----------------------------------------------

    /// Activation funnel for users who signed up in `hours` (clamped [1,720]):
    /// per-user signup → first-engagement, with session/ping counts. Port of
    /// `getActivationFunnel`.
    pub async fn get_activation_funnel(&self, hours: f64) -> Result<Value, sqlx::Error> {
        let h = clamp_hours(hours, 1.0, 720.0, 24.0);
        // +1ms so the half-open [since, until) window still includes an event
        // written in the current millisecond (otherwise "last N hours" silently
        // drops the newest row — and made funnel_report_distinct_installs_ordered flaky).
        let until = now_ms() + 1;
        let since = until - (h * HOUR_MS_F) as i64;
        self.get_activation_funnel_window(since, until).await
    }

    /// Windowed activation funnel over `[since, until)` by signup time. Powers the
    /// dashboard picker (presets / custom range / all).
    pub async fn get_activation_funnel_window(
        &self,
        since: i64,
        until: i64,
    ) -> Result<Value, sqlx::Error> {
        self.get_activation_funnel_window_for_variant(since, until, None).await
    }

    /// Activation limited to one experiment arm. Signups with no linked install
    /// have no arm, so a filtered view drops them.
    pub async fn get_activation_funnel_window_for_variant(
        &self,
        since: i64,
        until: i64,
        variant: Option<&str>,
    ) -> Result<Value, sqlx::Error> {
        let engaged_names = engaged_in_list();
        let sql = format!(
            "SELECT
               u.name,
               u.email,
               u.install_id,
               u.version,
               u.os_version    AS os,
               u.country,
               u.created_at    AS signed_up_at,
               (SELECT MIN(e.created_at) FROM events e
                 WHERE e.install_id = u.install_id
                   AND e.name IN ({engaged_names})
                   AND e.created_at >= u.created_at) AS first_activation_at,
               (SELECT COUNT(*) FROM events e
                 WHERE e.install_id = u.install_id
                   AND e.name = 'session.started'
                   AND e.created_at >= u.created_at) AS session_count,
               (SELECT COALESCE(SUM(CAST(json_extract(e.props, '$.duration_ms') AS INTEGER)), 0)
                 FROM events e
                 WHERE e.install_id = u.install_id
                   AND e.name = 'session.ended'
                   AND e.created_at >= u.created_at) AS total_session_ms,
               (SELECT COUNT(*) FROM pings p
                 WHERE p.install_id = u.install_id
                   AND p.created_at >= u.created_at) AS ping_count
             FROM users u
             WHERE u.created_at >= ? AND u.created_at < ?
             ORDER BY u.created_at DESC"
        );
        let rows = sqlx::query(&sql).bind(since).bind(until).fetch_all(self.pool()).await?;

        let mut users: Vec<Value> = Vec::with_capacity(rows.len());
        let mut activated_total = 0i64;
        for r in &rows {
            let iid: Option<String> = r.get("install_id");
            let iid = iid.unwrap_or_default();
            if variant.is_some() && !crate::variant::matches(variant, &iid) {
                continue;
            }
            let signed_up_at: i64 = r.get("signed_up_at");
            let first_activation_at: Option<i64> = r.get("first_activation_at");
            let activated = first_activation_at.is_some();
            if activated {
                activated_total += 1;
            }
            // u.name/version/os/country are nullable TEXT; read as Option to
            // serialize null exactly like JS passes `r.col` through.
            let name: Option<String> = r.get("name");
            let email: Option<String> = r.get("email");
            let install_id: Option<String> = r.get("install_id");
            let version: Option<String> = r.get("version");
            let os: Option<String> = r.get("os");
            let country: Option<String> = r.get("country");
            let session_count: i64 = r.get("session_count");
            let total_session_ms: i64 = r.get("total_session_ms");
            let ping_count: i64 = r.get("ping_count");

            users.push(json!({
                "name": name,
                "email": email,
                "install_id": install_id,
                "version": version,
                "os": os,
                "country": country,
                "signed_up_at": iso_ms(signed_up_at),
                "activated": activated,
                "first_activation_at": first_activation_at.map(iso_ms),
                "time_to_activation_ms": first_activation_at.map(|a| a - signed_up_at),
                "session_count": session_count,
                "total_session_ms": total_session_ms,
                "ping_count": ping_count,
                "variant": crate::variant::variant_for(&iid),
            }));
        }

        let total = users.len() as i64;
        let activation_rate = if total > 0 {
            js_round2((activated_total as f64) / (total as f64))
        } else {
            Value::Null
        };

        Ok(json!({
            "total": total,
            "activated": activated_total,
            "activation_rate": activation_rate,
            "window_hours": json_num((until - since) as f64 / HOUR_MS_F),
            "variant": variant,
            "users": users,
        }))
    }

    // ---- getFunnelReport --------------------------------------------------

    /// Per-step funnel conversion over `funnel.%` events, distinct installs,
    /// ordered installs desc then name asc. Port of `getFunnelReport`.
    pub async fn get_funnel_report(&self, hours: f64) -> Result<Value, sqlx::Error> {
        let h = clamp_hours(hours, 1.0, 720.0, 24.0);
        // +1ms so the half-open [since, until) window still includes an event
        // written in the current millisecond (otherwise "last N hours" silently
        // drops the newest row — and made funnel_report_distinct_installs_ordered flaky).
        let until = now_ms() + 1;
        let since = until - (h * HOUR_MS_F) as i64;
        self.get_funnel_report_window(since, until).await
    }

    /// Windowed funnel over `[since, until)`. Powers the dashboard picker.
    pub async fn get_funnel_report_window(
        &self,
        since: i64,
        until: i64,
    ) -> Result<Value, sqlx::Error> {
        self.get_funnel_report_window_for_variant(since, until, None).await
    }

    /// Funnel limited to one experiment arm, plus the Exp 1 ratios.
    ///
    /// `steps`: distinct installs per `funnel.%` event in the window (same
    /// shape and order as before). `metrics`: the ratios Research reads —
    /// first_chat_attempt / app_launched, chat_blocked_on_gateway /
    /// first_chat_attempt, onboarding_completed / app_launched — and
    /// `first_success_24h`: installs whose FIRST `funnel.app_launched` ever is
    /// in the window, how many have had 24h to mature, and how many of those
    /// sent `funnel.first_success` within 24h of that launch.
    pub async fn get_funnel_report_window_for_variant(
        &self,
        since: i64,
        until: i64,
        variant: Option<&str>,
    ) -> Result<Value, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT DISTINCT name, install_id
             FROM events
             WHERE name LIKE 'funnel.%' AND created_at >= ? AND created_at < ?",
        )
        .bind(since)
        .bind(until)
        .fetch_all(self.pool())
        .await?;
        let mut counts: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        for r in &rows {
            let iid: String = r.get("install_id");
            if crate::variant::matches(variant, &iid) {
                *counts.entry(r.get::<String, _>("name")).or_default() += 1;
            }
        }
        let mut ordered: Vec<(String, i64)> = counts.clone().into_iter().collect();
        ordered.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let steps: Vec<Value> = ordered
            .iter()
            .map(|(name, installs)| json!({ "name": name, "installs": installs }))
            .collect();

        let n = |k: &str| counts.get(k).copied().unwrap_or(0);
        let ratio = |num: i64, den: i64| -> Value {
            if den > 0 {
                js_round2(num as f64 / den as f64)
            } else {
                Value::Null
            }
        };
        let launched = n("funnel.app_launched");
        let attempted = n("funnel.first_chat_attempt");

        // First-ever launch per install, cohort = first launch inside the window.
        let fs = sqlx::query(
            "SELECT l.install_id AS install_id, l.first AS first,
                    (SELECT MIN(e.created_at) FROM events e
                      WHERE e.install_id = l.install_id AND e.name = ?) AS success_at
               FROM (SELECT install_id, MIN(created_at) AS first FROM events
                      WHERE name = 'funnel.app_launched' GROUP BY install_id) l
              WHERE l.first >= ? AND l.first < ?",
        )
        .bind(crate::variant::FIRST_SUCCESS)
        .bind(since)
        .bind(until)
        .fetch_all(self.pool())
        .await?;
        let now = now_ms();
        let (mut cohort, mut matured, mut reached) = (0i64, 0i64, 0i64);
        for r in &fs {
            let iid: String = r.get("install_id");
            if !crate::variant::matches(variant, &iid) {
                continue;
            }
            cohort += 1;
            let first: i64 = r.get("first");
            if now < first + DAY_MS {
                continue;
            }
            matured += 1;
            let success_at: Option<i64> = r.get("success_at");
            if success_at.is_some_and(|t| t <= first + DAY_MS) {
                reached += 1;
            }
        }

        Ok(json!({
            "window_hours": json_num((until - since) as f64 / HOUR_MS_F),
            "variant": variant,
            "steps": steps,
            "metrics": {
                "first_chat_attempt_per_app_launched": ratio(attempted, launched),
                "chat_blocked_on_gateway_per_first_chat_attempt":
                    ratio(n("funnel.chat_blocked_on_gateway"), attempted),
                "onboarding_completed_per_app_launched":
                    ratio(n("funnel.onboarding_completed"), launched),
                "first_success_24h": {
                    "launched": cohort,
                    "matured": matured,
                    "reached": reached,
                    "rate": ratio(reached, matured),
                },
            },
        }))
    }

    // ---- getCrashReport ---------------------------------------------------

    /// Crash + hang report grouped by version / signal / top stack frame over
    /// `hours` (clamped [1,2160]). Port of `getCrashReport`, including the exact
    /// substr/instr/char(10) top-frame expression.
    pub async fn get_crash_report(&self, hours: f64) -> Result<Value, sqlx::Error> {
        let h = clamp_hours(hours, 1.0, 2160.0, 168.0);
        let since = now_ms_f() - h * HOUR_MS_F;
        let rows = sqlx::query(
            "SELECT
               COALESCE(version, 'unknown')                            AS version,
               json_extract(props, '$.signal')                        AS signal,
               TRIM(substr(
                 COALESCE(json_extract(props, '$.stack'), ''),
                 1,
                 CASE
                   WHEN instr(COALESCE(json_extract(props, '$.stack'), ''), char(10)) > 0
                   THEN instr(COALESCE(json_extract(props, '$.stack'), ''), char(10)) - 1
                   ELSE length(COALESCE(json_extract(props, '$.stack'), ''))
                 END
               ))                                                      AS top_frame,
               COUNT(*)                                                AS n,
               COUNT(DISTINCT install_id)                             AS installs
             FROM events
             WHERE (name = 'error.crash' OR name LIKE 'error.hang%')
               AND created_at >= ?
             GROUP BY version, signal, top_frame
             ORDER BY version DESC, n DESC",
        )
        .bind(since)
        .fetch_all(self.pool())
        .await?;
        let out: Vec<Value> = rows
            .iter()
            .map(|r| {
                // top_frame: JS `r.top_frame || null` — empty string becomes null.
                let top_frame: Option<String> = r.get("top_frame");
                let top_frame = match top_frame {
                    Some(s) if !s.is_empty() => Value::String(s),
                    _ => Value::Null,
                };
                json!({
                    "version": r.get::<String, _>("version"),
                    "signal": json_value(r, "signal"),
                    "top_frame": top_frame,
                    "count": r.get::<i64, _>("n"),
                    "installs": r.get::<i64, _>("installs"),
                })
            })
            .collect();
        Ok(json!({ "window_hours": json_num(h), "rows": out }))
    }

    // ---- getNewErrorSignatures --------------------------------------------

    /// `error.%` signatures (name, file, line) seen in the last 24h but NOT in the
    /// prior [weekAgo, dayAgo) baseline. Port of `getNewErrorSignatures`.
    pub async fn get_new_error_signatures(&self) -> Result<Value, sqlx::Error> {
        let now = now_ms();
        let day_ago = now - DAY_MS;
        let week_ago = now - 7 * DAY_MS;
        let rows = sqlx::query(
            "SELECT
               name,
               json_extract(props, '$.file') AS file,
               json_extract(props, '$.line') AS line,
               COUNT(*)                      AS n,
               COUNT(DISTINCT install_id)    AS installs
             FROM events
             WHERE name LIKE 'error.%'
               AND created_at >= ?
             GROUP BY name, file, line
             HAVING NOT EXISTS (
               SELECT 1 FROM events prior
               WHERE prior.name LIKE 'error.%'
                 AND prior.created_at >= ?
                 AND prior.created_at < ?
                 AND prior.name = events.name
                 AND json_extract(prior.props, '$.file') IS file
                 AND json_extract(prior.props, '$.line') IS line
             )
             ORDER BY n DESC",
        )
        .bind(day_ago)
        .bind(week_ago)
        .bind(day_ago)
        .fetch_all(self.pool())
        .await?;
        let out: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "name": r.get::<String, _>("name"),
                    "file": json_value(r, "file"),
                    "line": json_value(r, "line"),
                    "count": r.get::<i64, _>("n"),
                    "installs": r.get::<i64, _>("installs"),
                })
            })
            .collect();
        Ok(json!({ "rows": out }))
    }

    // ---- getDiagnosticsList -----------------------------------------------

    /// Recent diagnostic submissions (newest first), limit clamped [1,500], with
    /// the fingerprint JSON parsed tolerantly. Port of `getDiagnosticsList`.
    pub async fn get_diagnostics_list(&self, limit: f64) -> Result<Vec<Value>, sqlx::Error> {
        // JS: const n = Number(limit); Number.isFinite(n) ? Math.max(1,
        // Math.min(500, Math.floor(n))) : 50. The ONLY clamp that floors.
        let lim: i64 = if limit.is_finite() {
            (limit.floor() as i64).clamp(1, 500)
        } else {
            50
        };
        let rows = sqlx::query(
            "SELECT id, install_id, version, created_at, note, fingerprint
             FROM diagnostics
             ORDER BY created_at DESC, id DESC
             LIMIT ?",
        )
        .bind(lim)
        .fetch_all(self.pool())
        .await?;
        Ok(rows
            .iter()
            .map(|r| {
                let version: Option<String> = r.get("version");
                let note: Option<String> = r.get("note");
                let fingerprint: Option<String> = r.get("fingerprint");
                json!({
                    "id": r.get::<i64, _>("id"),
                    "install_id": r.get::<String, _>("install_id"),
                    "version": version,
                    "created_at": r.get::<i64, _>("created_at"),
                    "note": note,
                    "fingerprint": parse_json_or_null(fingerprint),
                })
            })
            .collect())
    }

    // ---- getDiagnostic ----------------------------------------------------

    /// Single diagnostic by id, with all three JSON columns parsed tolerantly.
    /// `None` when the id doesn't exist. Port of `getDiagnostic` (the non-integer
    /// guard is enforced at the route boundary via the `Path<i64>` extractor).
    pub async fn get_diagnostic(&self, id: i64) -> Result<Option<Value>, sqlx::Error> {
        let row: Option<SqliteRow> = sqlx::query(
            "SELECT id, install_id, version, fingerprint, logs, gateway, note, created_at
             FROM diagnostics WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        let Some(r) = row else { return Ok(None) };
        let version: Option<String> = r.get("version");
        let note: Option<String> = r.get("note");
        let fingerprint: Option<String> = r.get("fingerprint");
        let logs: Option<String> = r.get("logs");
        let gateway: Option<String> = r.get("gateway");
        Ok(Some(json!({
            "id": r.get::<i64, _>("id"),
            "install_id": r.get::<String, _>("install_id"),
            "version": version,
            "created_at": r.get::<i64, _>("created_at"),
            "note": note,
            "fingerprint": parse_json_or_null(fingerprint),
            "logs": parse_json_or_null(logs),
            "gateway": parse_json_or_null(gateway),
        })))
    }

    // ---- small query helper ----------------------------------------------

    /// Scalar `COUNT(*)`-style query returning a single `i64`. `binds` are bound
    /// in order (all `i64` — the only param type the report counts use).
    async fn scalar_i64(&self, sql: &str, binds: &[i64]) -> Result<i64, sqlx::Error> {
        let mut q = sqlx::query_scalar::<_, i64>(sql);
        for b in binds {
            q = q.bind(*b);
        }
        q.fetch_one(self.pool()).await
    }

    /// `ACTIVE_INSTALLS` grouped per install, timestamps ascending (never empty).
    /// One scan, so retention/lifecycle bucket in Rust instead of a query per member.
    async fn activity_by_install(
        &self,
    ) -> Result<std::collections::HashMap<String, Vec<i64>>, sqlx::Error> {
        let rows = sqlx::query(&format!("SELECT install_id, created_at FROM {ACTIVE_INSTALLS} AS a"))
            .fetch_all(self.pool())
            .await?;
        let mut out: std::collections::HashMap<String, Vec<i64>> = std::collections::HashMap::new();
        for r in &rows {
            out.entry(r.get("install_id")).or_default().push(r.get("created_at"));
        }
        for ts in out.values_mut() {
            ts.sort_unstable();
        }
        Ok(out)
    }

    // ---- getIssues (Phase 1) ----------------------------------------------

    /// Error-issue list (newest activity first, top 200), optionally filtered to
    /// one status. Each row is a distinct error signature with its lifecycle
    /// state. Timestamps are raw epoch-ms (like getDiagnosticsList).
    pub async fn get_issues(&self, status: Option<&str>) -> Result<Value, sqlx::Error> {
        const SELECT: &str = "SELECT id, fingerprint, name, file, function, line, status, count, first_seen, last_seen, last_version, resolved_at, resolved_in_version FROM issues";
        let rows = match status {
            Some(s) => {
                sqlx::query(&format!("{SELECT} WHERE status = ? ORDER BY last_seen DESC LIMIT 200"))
                    .bind(s)
                    .fetch_all(self.pool())
                    .await?
            }
            None => {
                sqlx::query(&format!("{SELECT} ORDER BY last_seen DESC LIMIT 200"))
                    .fetch_all(self.pool())
                    .await?
            }
        };
        let out: Vec<Value> = rows.iter().map(issue_json).collect();
        Ok(json!({ "rows": out }))
    }

    /// A single issue by id as JSON, or None. Used by `set_issue_status` to echo
    /// the updated row back to the PATCH caller.
    pub async fn get_issue_by_id(&self, id: i64) -> Result<Option<Value>, sqlx::Error> {
        let row: Option<SqliteRow> = sqlx::query(
            "SELECT id, fingerprint, name, file, function, line, status, count, first_seen, last_seen, last_version, resolved_at, resolved_in_version FROM issues WHERE id = ?",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.as_ref().map(issue_json))
    }

    // ---- getUsersList (Phase 2 dashboard) ---------------------------------

    /// Full user list for the dashboard's Users tab: one row per user with
    /// lifetime engagement (chat msgs + voice), distinct active days, current
    /// version, signup, and last-seen (max ping). Newest activity first; `limit`
    /// floored + clamped [1, 1000] (default 200). Timestamps are ISO strings.
    pub async fn get_users_list(&self, limit: f64) -> Result<Value, sqlx::Error> {
        let lim: i64 = if limit.is_finite() {
            (limit.floor() as i64).clamp(1, 1000)
        } else {
            200
        };
        let total = self.scalar_i64("SELECT COUNT(*) AS n FROM users", &[]).await?;
        let engaged_chat = "chat-sent";
        let engaged_voice = "voice.activated";
        let rows = sqlx::query(
            "SELECT
               u.name, u.email, u.install_id, u.version, u.os_version AS os, u.country,
               u.created_at AS signed_up_at,
               (SELECT MAX(p.created_at) FROM pings p WHERE p.install_id = u.install_id) AS last_seen,
               (SELECT COUNT(*) FROM events e WHERE e.install_id = u.install_id AND e.name = ?) AS msgs,
               (SELECT COUNT(*) FROM events e WHERE e.install_id = u.install_id AND e.name = ?) AS voice,
               (SELECT COUNT(DISTINCT p.created_at / 86400000) FROM pings p WHERE p.install_id = u.install_id) AS active_days
             FROM users u
             ORDER BY last_seen IS NULL, last_seen DESC, u.created_at DESC
             LIMIT ?",
        )
        .bind(engaged_chat)
        .bind(engaged_voice)
        .bind(lim)
        .fetch_all(self.pool())
        .await?;
        let out: Vec<Value> = rows
            .iter()
            .map(|r| {
                let name: Option<String> = r.get("name");
                let email: Option<String> = r.get("email");
                let install_id: Option<String> = r.get("install_id");
                let version: Option<String> = r.get("version");
                let os: Option<String> = r.get("os");
                let country: Option<String> = r.get("country");
                let signed_up_at: i64 = r.get("signed_up_at");
                let last_seen: Option<i64> = r.get("last_seen");
                json!({
                    "name": name,
                    "email": email,
                    "install_id": install_id,
                    "version": version,
                    "os": os,
                    "country": country,
                    "signed_up_at": iso_ms(signed_up_at),
                    "last_seen": last_seen.map(iso_ms),
                    "msgs": r.get::<i64, _>("msgs"),
                    "voice": r.get::<i64, _>("voice"),
                    "active_days": r.get::<i64, _>("active_days"),
                })
            })
            .collect();
        Ok(json!({ "total": total, "rows": out }))
    }

    // ---- getDailyActivity (daily report person lists) ---------------------

    /// 24h new-signups + last-seen active lists for the daily report composite.
    /// Operator noise (hardcoded install/email) and smoke-test pattern rows are
    /// omitted so the person lists match the Claude desktop runbook. Totals in
    /// `get_stats` are unchanged. When both 24h lists are empty, includes a 7d
    /// signups list + 10 most-recent-seen for context.
    pub async fn get_daily_activity(&self) -> Result<Value, sqlx::Error> {
        let now = now_ms();
        let day_ago = now - DAY_MS;
        let week_ago = now - 7 * DAY_MS;
        let rows = sqlx::query(
            "SELECT
               u.name, u.email, u.install_id, u.version,
               u.created_at AS signed_up_at,
               (SELECT MAX(p.created_at) FROM pings p WHERE p.install_id = u.install_id) AS last_seen,
               (SELECT COUNT(*) FROM events e WHERE e.install_id = u.install_id AND e.name = 'chat-sent') AS msgs
             FROM users u",
        )
        .fetch_all(self.pool())
        .await?;

        let mut people: Vec<(i64, Option<i64>, Value)> = Vec::new();
        for r in &rows {
            let install_id: Option<String> = r.get("install_id");
            let email: Option<String> = r.get("email");
            if Db::is_smoke_test(install_id.as_deref(), email.as_deref()) {
                continue;
            }
            if self.report_exclude().matches(email.as_deref(), install_id.as_deref()) {
                continue;
            }
            let signed_up_at: i64 = r.get("signed_up_at");
            let last_seen: Option<i64> = r.get("last_seen");
            let name: Option<String> = r.get("name");
            let version: Option<String> = r.get("version");
            let msgs: i64 = r.get("msgs");
            people.push((
                signed_up_at,
                last_seen,
                json!({
                    "name": name,
                    "email": email,
                    "install_id": install_id,
                    "version": version,
                    "signed_up_at": iso_ms(signed_up_at),
                    "last_seen": last_seen.map(iso_ms),
                    "msgs": msgs,
                }),
            ));
        }

        let new_signups_24h: Vec<Value> = people
            .iter()
            .filter(|(su, _, _)| *su >= day_ago)
            .map(|(_, _, v)| v.clone())
            .collect();
        let active_24h: Vec<Value> = people
            .iter()
            .filter(|(_, ls, _)| ls.is_some_and(|t| t >= day_ago))
            .map(|(_, _, v)| v.clone())
            .collect();

        let mut out = json!({
            "new_signups_24h": new_signups_24h,
            "active_24h": active_24h,
            "window_hours": 24,
        });

        if out["new_signups_24h"]
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(true)
            && out["active_24h"]
                .as_array()
                .map(|a| a.is_empty())
                .unwrap_or(true)
        {
            let mut signups_7d: Vec<(i64, Value)> = people
                .iter()
                .filter(|(su, _, _)| *su >= week_ago)
                .map(|(su, _, v)| (*su, v.clone()))
                .collect();
            signups_7d.sort_by(|a, b| b.0.cmp(&a.0));
            let signups_7d: Vec<Value> = signups_7d.into_iter().map(|(_, v)| v).collect();

            let mut recent: Vec<(i64, Value)> = people
                .iter()
                .filter_map(|(_, ls, v)| ls.map(|t| (t, v.clone())))
                .collect();
            recent.sort_by(|a, b| b.0.cmp(&a.0));
            let recent_seen: Vec<Value> = recent.into_iter().take(10).map(|(_, v)| v).collect();

            out["fallback"] = json!({
                "signups_7d": signups_7d,
                "recent_seen": recent_seen,
            });
        }

        Ok(out)
    }

    // ---- getLifecycle (Phase 1) -------------------------------------------

    /// Weekly lifecycle buckets (PostHog-style) over ping activity: for each of
    /// the most recent `weeks` (clamped [1,52], default 12), how many installs are
    /// new / returning / resurrecting / dormant. "Active in week W" = any `ACTIVE_INSTALLS` row in W.
    ///   - new:          first-ever ping is in W
    ///   - returning:    active in W and in W-1 (not new)
    ///   - resurrecting: active in W, not in W-1, but active at some earlier week
    ///   - dormant:      NOT active in W but was active in W-1
    /// Computed in Rust from distinct (install, week) pairs — same shape as
    /// getRetention (pull rows, bucket in code), no window functions needed.
    pub async fn get_lifecycle(&self, weeks: f64) -> Result<Value, sqlx::Error> {
        let n = clamp_hours(weeks, 1.0, 52.0, 12.0) as i64;
        let now = now_ms();
        let week_ms = 7 * DAY_MS;
        let current_week = week_start_ms(now);

        // Per-install: the set of active week-starts + earliest week.
        let active: std::collections::HashMap<String, std::collections::HashSet<i64>> = self
            .activity_by_install()
            .await?
            .into_iter()
            .map(|(iid, ts)| (iid, ts.into_iter().map(week_start_ms).collect()))
            .collect();
        let first_week: std::collections::HashMap<&String, i64> = active
            .iter()
            .map(|(iid, weeks)| (iid, *weeks.iter().min().unwrap_or(&i64::MAX)))
            .collect();

        // Weeks ascending: current_week - (n-1)*week .. current_week.
        let mut out: Vec<Value> = Vec::with_capacity(n as usize);
        for i in (0..n).rev() {
            let w = current_week - i * week_ms;
            let prev = w - week_ms;
            let (mut new_, mut returning, mut resurrecting, mut dormant) = (0i64, 0i64, 0i64, 0i64);
            for (iid, weeks) in &active {
                let now_active = weeks.contains(&w);
                let prev_active = weeks.contains(&prev);
                let is_new = first_week.get(iid).copied().unwrap_or(i64::MAX) == w;
                if now_active && is_new {
                    new_ += 1;
                } else if now_active && prev_active {
                    returning += 1;
                } else if now_active && !prev_active {
                    resurrecting += 1;
                } else if !now_active && prev_active {
                    dormant += 1;
                }
            }
            out.push(json!({
                "week": iso_date(w),
                "new": new_,
                "returning": returning,
                "resurrecting": resurrecting,
                "dormant": dormant,
            }));
        }
        Ok(json!({ "weeks": out }))
    }

    // ---- getAttribution (Phase 1) -----------------------------------------

    /// Signups grouped by landing UTM, via users → install_landing_map →
    /// landing_visits. Unmapped installs (no forwarded iid, or no matching
    /// landing visit) bucket as source='direct'. Windowed by signup time
    /// (`hours`, clamped [1, 8760], default 720 = 30d).
    pub async fn get_attribution(&self, hours: f64) -> Result<Value, sqlx::Error> {
        let h = clamp_hours(hours, 1.0, 8760.0, 720.0);
        // +1ms so the half-open [since, until) window still includes an event
        // written in the current millisecond (otherwise "last N hours" silently
        // drops the newest row — and made funnel_report_distinct_installs_ordered flaky).
        let until = now_ms() + 1;
        let since = until - (h * HOUR_MS_F) as i64;
        self.get_attribution_window(since, until).await
    }

    /// Windowed signup attribution over `[since, until)`. Powers the picker.
    pub async fn get_attribution_window(&self, since: i64, until: i64) -> Result<Value, sqlx::Error> {
        let total: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE created_at >= ? AND created_at < ?")
                .bind(since)
                .bind(until)
                .fetch_one(self.pool())
                .await?;
        let rows = sqlx::query(
            "SELECT
               COALESCE(lv.utm_source, 'direct') AS source,
               COALESCE(lv.utm_medium, 'none')   AS medium,
               COALESCE(lv.utm_campaign, 'none') AS campaign,
               COUNT(*)                          AS signups
             FROM users u
             LEFT JOIN install_landing_map m ON m.install_id = u.install_id
             LEFT JOIN landing_visits lv      ON lv.iid = m.iid
             WHERE u.created_at >= ? AND u.created_at < ?
             GROUP BY source, medium, campaign
             ORDER BY signups DESC",
        )
        .bind(since)
        .bind(until)
        .fetch_all(self.pool())
        .await?;
        let out: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "source": r.get::<String, _>("source"),
                    "medium": r.get::<String, _>("medium"),
                    "campaign": r.get::<String, _>("campaign"),
                    "signups": r.get::<i64, _>("signups"),
                })
            })
            .collect();
        Ok(json!({
            "window_hours": json_num((until - since) as f64 / HOUR_MS_F),
            "total": total,
            "rows": out,
        }))
    }

    // ---- getBreakdown (Phase 1) -------------------------------------------

    /// User + engaged-user counts grouped by one dimension of the users table:
    /// `version`, `os` (os_version), or `country`. `by` is validated at the route
    /// (only these three reach here), then whitelisted to a real column name here
    /// so the interpolation is safe. yagni: a single breakdown endpoint over
    /// users, not a `?by=` param bolted onto every time-series report.
    pub async fn get_breakdown(&self, by: &str) -> Result<Value, sqlx::Error> {
        let col = match by {
            "version" => "version",
            "os" => "os_version",
            "country" => "country",
            // Unreachable: the route rejects other values with a 400 before here.
            _ => return Ok(json!({ "by": by, "rows": [] })),
        };
        let sql = format!(
            "SELECT COALESCE(u.{col}, 'unknown') AS bucket,
               COUNT(*) AS users,
               SUM(CASE WHEN EXISTS (
                 SELECT 1 FROM events e WHERE e.install_id = u.install_id AND e.name IN ({})
               ) THEN 1 ELSE 0 END) AS engaged
             FROM users u
             GROUP BY bucket
             ORDER BY users DESC",
            engaged_in_list()
        );
        let rows = sqlx::query(&sql).fetch_all(self.pool()).await?;
        let out: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "bucket": r.get::<String, _>("bucket"),
                    "users": r.get::<i64, _>("users"),
                    "engaged": r.get::<i64, _>("engaged"),
                })
            })
            .collect();
        Ok(json!({ "by": by, "rows": out }))
    }

    // ---- getLandingEvents (landing Umami mirror) --------------------------

    /// Named landing events grouped by name over the last `hours`: total count
    /// and unique visitors (distinct iid) per event name, most frequent first.
    pub async fn get_landing_events(&self, hours: f64) -> Result<Value, sqlx::Error> {
        let h = clamp_hours(hours, 1.0, 8760.0, 720.0);
        let since = now_ms_f() - h * HOUR_MS_F;
        let total: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM landing_events WHERE created_at >= ?")
                .bind(since)
                .fetch_one(self.pool())
                .await?;
        let rows = sqlx::query(
            "SELECT name,
                    COUNT(*)             AS count,
                    COUNT(DISTINCT iid)  AS visitors
             FROM landing_events
             WHERE created_at >= ?
             GROUP BY name
             ORDER BY count DESC",
        )
        .bind(since)
        .fetch_all(self.pool())
        .await?;
        let out: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "name": r.get::<String, _>("name"),
                    "count": r.get::<i64, _>("count"),
                    "visitors": r.get::<i64, _>("visitors"),
                })
            })
            .collect();
        Ok(json!({ "window_hours": json_num(h), "total": total, "rows": out }))
    }

    // ---- getLandingFunnel (landing Umami mirror) --------------------------

    /// Acquisition funnel over the last `hours`, each step a distinct-iid count:
    /// visited (landing_visits) → downloaded (a `download` landing_event) →
    /// installed (iid mapped to an install) → signed_up (that install became a
    /// user). yagni: per-stage window on each stage's own timestamp, not a strict
    /// same-visitor cohort — good enough for a top-of-funnel gauge.
    pub async fn get_landing_funnel(&self, hours: f64) -> Result<Value, sqlx::Error> {
        let h = clamp_hours(hours, 1.0, 8760.0, 720.0);
        let since = now_ms_f() - h * HOUR_MS_F;
        let visited: i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT iid) FROM landing_visits WHERE first_seen_at >= ?",
        )
        .bind(since)
        .fetch_one(self.pool())
        .await?;
        let downloaded: i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT iid) FROM landing_events WHERE name = 'download' AND created_at >= ?",
        )
        .bind(since)
        .fetch_one(self.pool())
        .await?;
        let installed: i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT iid) FROM install_landing_map WHERE matched_at >= ?",
        )
        .bind(since)
        .fetch_one(self.pool())
        .await?;
        let signed_up: i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT m.iid)
             FROM install_landing_map m
             JOIN users u ON u.install_id = m.install_id
             WHERE u.created_at >= ?",
        )
        .bind(since)
        .fetch_one(self.pool())
        .await?;
        let steps = json!([
            { "step": "visited", "count": visited },
            { "step": "downloaded", "count": downloaded },
            { "step": "installed", "count": installed },
            { "step": "signed_up", "count": signed_up },
        ]);
        Ok(json!({ "window_hours": json_num(h), "steps": steps }))
    }

    // ---- multi-tenant site analytics (Umami replacement) ------------------

    /// All sites with lifetime event + visitor counts and last-seen, busiest
    /// first. Powers the dashboard site switcher.
    pub async fn list_sites(&self) -> Result<Value, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT s.id, s.name, s.domain, s.write_key,
               (SELECT COUNT(*) FROM site_events e WHERE e.site_id = s.id AND COALESCE(e.bot,0)=0) AS events,
               (SELECT COUNT(DISTINCT COALESCE(e.session_id, e.distinct_id))
                  FROM site_events e WHERE e.site_id = s.id AND COALESCE(e.bot,0)=0) AS visitors,
               (SELECT MAX(created_at) FROM site_events e WHERE e.site_id = s.id AND COALESCE(e.bot,0)=0) AS last_event
             FROM sites s
             ORDER BY events DESC, s.name",
        )
        .fetch_all(self.pool())
        .await?;
        let sites: Vec<Value> = rows
            .iter()
            .map(|r| {
                let domain: Option<String> = r.get("domain");
                let last_event: Option<i64> = r.get("last_event");
                json!({
                    "id": r.get::<String, _>("id"),
                    "name": r.get::<String, _>("name"),
                    "domain": domain,
                    // Admin-gated response only — the write_key is the per-site
                    // ingest secret for /api/collect; exposed here so the
                    // dashboard can render each site's install snippet.
                    "write_key": r.get::<String, _>("write_key"),
                    "events": r.get::<i64, _>("events"),
                    "visitors": r.get::<i64, _>("visitors"),
                    "last_event": last_event,
                })
            })
            .collect();
        Ok(json!({ "sites": sites }))
    }

    /// Range KPI summary for one site (Umami-style): unique visitors, visits
    /// (sessions), pageviews, bounce rate, avg visit duration, custom events.
    /// Window is `[since_ms, until_ms)`. Also returns `prev` for the equal-length
    /// window immediately before (for period-over-period deltas on the dashboard).
    pub async fn get_site_summary(
        &self,
        site_id: &str,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Value, sqlx::Error> {
        let cur = self.site_window_stats(site_id, since_ms, until_ms).await?;
        let span = (until_ms - since_ms).max(DAY_MS);
        let prev = self
            .site_window_stats(site_id, since_ms - span, since_ms)
            .await?;
        Ok(json!({
            "visitors": cur["visitors"],
            "visits": cur["visits"],
            "views": cur["views"],
            "bounce_rate": cur["bounce_rate"],
            "visit_duration_ms": cur["visit_duration_ms"],
            "events": cur["events"],
            "prev": prev,
        }))
    }

    /// KPI numbers for one site over `[since_ms, until_ms)`.
    async fn site_window_stats(
        &self,
        site_id: &str,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Value, sqlx::Error> {
        // Visitors = distinct ids that hit a pageview (not identify-only events),
        // so visitors never exceeds pageviews. Humans only (bot=0).
        let visitors: i64 = sqlx::query_scalar(
            "SELECT COUNT(DISTINCT distinct_id) FROM site_events
             WHERE site_id = ? AND created_at >= ? AND created_at < ?
               AND event_type = 1 AND COALESCE(bot, 0) = 0
               AND distinct_id IS NOT NULL AND distinct_id <> ''",
        )
        .bind(site_id)
        .bind(since_ms)
        .bind(until_ms)
        .fetch_one(self.pool())
        .await?;

        let views: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM site_events
             WHERE site_id = ? AND created_at >= ? AND created_at < ?
               AND event_type = 1 AND COALESCE(bot, 0) = 0",
        )
        .bind(site_id)
        .bind(since_ms)
        .bind(until_ms)
        .fetch_one(self.pool())
        .await?;

        let events: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM site_events
             WHERE site_id = ? AND created_at >= ? AND created_at < ?
               AND event_type = 2 AND COALESCE(bot, 0) = 0",
        )
        .bind(site_id)
        .bind(since_ms)
        .bind(until_ms)
        .fetch_one(self.pool())
        .await?;

        // One row per visit (session_id, else distinct_id). Bounce = single pageview.
        // Duration = last−first event in the visit (0 when only one event).
        let sess = sqlx::query(
            "SELECT COUNT(*) AS visits,
                    COALESCE(SUM(CASE WHEN pageviews = 1 THEN 1 ELSE 0 END), 0) AS bounces,
                    COALESCE(AVG(CASE WHEN last_at > first_at THEN last_at - first_at ELSE 0 END), 0.0)
                      AS avg_duration_ms
             FROM (
               SELECT COALESCE(session_id, distinct_id) AS sid,
                      SUM(CASE WHEN event_type = 1 THEN 1 ELSE 0 END) AS pageviews,
                      MIN(created_at) AS first_at,
                      MAX(created_at) AS last_at
               FROM site_events
               WHERE site_id = ? AND created_at >= ? AND created_at < ?
                 AND COALESCE(bot, 0) = 0
                 AND COALESCE(session_id, distinct_id) IS NOT NULL
                 AND COALESCE(session_id, distinct_id) <> ''
               GROUP BY sid
             )",
        )
        .bind(site_id)
        .bind(since_ms)
        .bind(until_ms)
        .fetch_one(self.pool())
        .await?;

        let visits: i64 = sess.get("visits");
        let bounces: i64 = sess.get("bounces");
        let avg_duration_ms: f64 = sess.get("avg_duration_ms");
        let bounce_rate = if visits > 0 {
            js_round2((bounces as f64) / (visits as f64))
        } else {
            Value::Null
        };

        Ok(json!({
            "visitors": visitors,
            "visits": visits,
            "views": views,
            "bounce_rate": bounce_rate,
            "visit_duration_ms": avg_duration_ms.round() as i64,
            "events": events,
        }))
    }

    /// All-sites daily roll-up: one KPI row per site over the last `hours`
    /// (clamped [1,720], default 24) plus cross-site totals. The multi-tenant
    /// analog of the desktop `get_stats`/report — powers `/api/report/site-daily`.
    /// Sites are ordered busiest-first (views desc, then name).
    pub async fn get_sites_daily(&self, hours: f64) -> Result<Value, sqlx::Error> {
        let h = clamp_hours(hours, 1.0, 720.0, 24.0);
        // +1ms so the half-open [since, until) window still includes an event
        // written in the current millisecond (otherwise "last N hours" silently
        // drops the newest row — and made funnel_report_distinct_installs_ordered flaky).
        let until = now_ms() + 1;
        let since = until - (h * HOUR_MS_F) as i64;

        let rows = sqlx::query("SELECT id, name, domain FROM sites ORDER BY name")
            .fetch_all(self.pool())
            .await?;

        let mut sites: Vec<Value> = Vec::with_capacity(rows.len());
        let (mut t_visitors, mut t_visits, mut t_views, mut t_events) = (0i64, 0i64, 0i64, 0i64);
        for r in &rows {
            let id: String = r.get("id");
            let name: String = r.get("name");
            let domain: Option<String> = r.get("domain");
            let s = self.site_window_stats(&id, since, until).await?;
            t_visitors += s["visitors"].as_i64().unwrap_or(0);
            t_visits += s["visits"].as_i64().unwrap_or(0);
            t_views += s["views"].as_i64().unwrap_or(0);
            t_events += s["events"].as_i64().unwrap_or(0);
            sites.push(json!({
                "id": id,
                "name": name,
                "domain": domain,
                "visitors": s["visitors"],
                "visits": s["visits"],
                "views": s["views"],
                "bounce_rate": s["bounce_rate"],
                "visit_duration_ms": s["visit_duration_ms"],
                "events": s["events"],
            }));
        }
        // Busiest first: views desc, then name asc (stable input order is by name).
        sites.sort_by(|a, b| {
            b["views"]
                .as_i64()
                .unwrap_or(0)
                .cmp(&a["views"].as_i64().unwrap_or(0))
        });

        Ok(json!({
            "window_hours": json_num(h),
            "since_ms": since,
            "until_ms": until,
            "totals": {
                "sites": rows.len() as i64,
                "visitors": t_visitors,
                "visits": t_visits,
                "views": t_views,
                "events": t_events,
            },
            "sites": sites,
        }))
    }

    /// Per-UTC-day series for one site: `pageviews` (event_type 1), `events`
    /// (event_type 2), `visitors` (distinct visitor), or `visits` (sessions).
    /// Ascending, contiguous, zero-filled — same day-boundary math as `get_series`.
    pub async fn get_site_series(
        &self,
        site_id: &str,
        metric: &str,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Value, sqlx::Error> {
        // Metric validated at the route; fallback keeps the match total.
        let (count_expr, filter) = match metric {
            "pageviews" => ("COUNT(*)", " AND event_type = 1 AND COALESCE(bot,0)=0"),
            "events" => ("COUNT(*)", " AND event_type = 2 AND COALESCE(bot,0)=0"),
            // Pageview-only visitors so series matches summary KPI.
            "visitors" => (
                "COUNT(DISTINCT distinct_id)",
                " AND event_type = 1 AND COALESCE(bot,0)=0 AND distinct_id IS NOT NULL AND distinct_id <> ''",
            ),
            "visits" => (
                "COUNT(DISTINCT COALESCE(session_id, distinct_id))",
                " AND COALESCE(bot,0)=0 AND COALESCE(session_id, distinct_id) IS NOT NULL AND COALESCE(session_id, distinct_id) <> ''",
            ),
            _ => ("COUNT(*)", " AND COALESCE(bot,0)=0"),
        };
        let sql = format!(
            "SELECT created_at / {DAY_MS} AS bucket, {count_expr} AS n \
             FROM site_events WHERE site_id = ? AND created_at >= ? AND created_at < ?{filter} \
             GROUP BY bucket ORDER BY bucket"
        );
        let rows = sqlx::query(&sql)
            .bind(site_id)
            .bind(since_ms)
            .bind(until_ms)
            .fetch_all(self.pool())
            .await?;
        let mut counts: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
        for r in &rows {
            counts.insert(r.get::<i64, _>("bucket"), r.get::<i64, _>("n"));
        }
        let first = since_ms / DAY_MS;
        let last = (until_ms - 1) / DAY_MS;
        let mut series: Vec<Value> = Vec::with_capacity((last - first + 1).max(0) as usize);
        let mut bucket = first;
        while bucket <= last {
            series.push(json!({
                "date": iso_date(bucket * DAY_MS),
                "count": counts.get(&bucket).copied().unwrap_or(0),
            }));
            bucket += 1;
        }
        Ok(json!({ "metric": metric, "series": series }))
    }

    /// Custom events (event_type 2) for one site grouped by name over
    /// `[since_ms, until_ms)`: count + unique visitors, most frequent first.
    pub async fn get_site_events(
        &self,
        site_id: &str,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Value, sqlx::Error> {
        let total: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM site_events
             WHERE site_id = ? AND event_type = 2 AND created_at >= ? AND created_at < ?
               AND COALESCE(bot,0)=0",
        )
        .bind(site_id)
        .bind(since_ms)
        .bind(until_ms)
        .fetch_one(self.pool())
        .await?;
        let rows = sqlx::query(
            "SELECT name,
                    COUNT(*)                                          AS count,
                    COUNT(DISTINCT COALESCE(session_id, distinct_id)) AS visitors
             FROM site_events
             WHERE site_id = ? AND event_type = 2 AND created_at >= ? AND created_at < ?
               AND name IS NOT NULL AND COALESCE(bot,0)=0
             GROUP BY name ORDER BY count DESC LIMIT 100",
        )
        .bind(site_id)
        .bind(since_ms)
        .bind(until_ms)
        .fetch_all(self.pool())
        .await?;
        let out: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "name": r.get::<String, _>("name"),
                    "count": r.get::<i64, _>("count"),
                    "visitors": r.get::<i64, _>("visitors"),
                })
            })
            .collect();
        Ok(json!({
            "window_hours": json_num((until_ms - since_ms) as f64 / HOUR_MS_F),
            "total": total,
            "rows": out,
        }))
    }

    /// Top pages for one site over `[since_ms, until_ms)`.
    /// `kind`: `url` (default full URL), `path` (strip query/hash), `entry` /
    /// `exit` (first/last pageview per visit).
    pub async fn get_site_pages(
        &self,
        site_id: &str,
        since_ms: i64,
        until_ms: i64,
        kind: &str,
    ) -> Result<Value, sqlx::Error> {
        // Path expression: strip ?query and #hash from url.
        let path_expr = "CASE
            WHEN instr(COALESCE(url, '/'), '?') > 0
              THEN substr(COALESCE(url, '/'), 1, instr(COALESCE(url, '/'), '?') - 1)
            WHEN instr(COALESCE(url, '/'), '#') > 0
              THEN substr(COALESCE(url, '/'), 1, instr(COALESCE(url, '/'), '#') - 1)
            ELSE COALESCE(url, '/')
          END";
        let page_expr = if kind == "url" {
            "COALESCE(url, '/')"
        } else {
            path_expr
        };

        let sql = match kind {
            "entry" | "exit" => {
                let order = if kind == "entry" { "ASC" } else { "DESC" };
                format!(
                    "SELECT page AS url, COUNT(*) AS views,
                            COUNT(DISTINCT sid) AS visitors
                     FROM (
                       SELECT COALESCE(session_id, distinct_id) AS sid,
                              {page_expr} AS page,
                              ROW_NUMBER() OVER (
                                PARTITION BY COALESCE(session_id, distinct_id)
                                ORDER BY created_at {order}
                              ) AS rn
                       FROM site_events
                       WHERE site_id = ? AND event_type = 1 AND COALESCE(bot,0)=0
                         AND created_at >= ? AND created_at < ?
                         AND COALESCE(session_id, distinct_id) IS NOT NULL
                         AND COALESCE(session_id, distinct_id) <> ''
                     )
                     WHERE rn = 1
                     GROUP BY page ORDER BY views DESC LIMIT 50"
                )
            }
            _ => format!(
                "SELECT {page_expr} AS url,
                        COUNT(*) AS views,
                        COUNT(DISTINCT COALESCE(session_id, distinct_id)) AS visitors
                 FROM site_events
                 WHERE site_id = ? AND event_type = 1 AND COALESCE(bot,0)=0
                   AND created_at >= ? AND created_at < ?
                 GROUP BY 1 ORDER BY views DESC LIMIT 50"
            ),
        };

        let rows = sqlx::query(&sql)
            .bind(site_id)
            .bind(since_ms)
            .bind(until_ms)
            .fetch_all(self.pool())
            .await?;
        let out: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "url": r.get::<String, _>("url"),
                    "views": r.get::<i64, _>("views"),
                    "visitors": r.get::<i64, _>("visitors"),
                })
            })
            .collect();
        Ok(json!({ "kind": kind, "rows": out }))
    }

    /// Dimension breakdown for one site over `[since_ms, until_ms)`.
    /// `by` ∈ browser | os | device | country. Rows: name, visitors, hits.
    pub async fn get_site_breakdown(
        &self,
        site_id: &str,
        since_ms: i64,
        until_ms: i64,
        by: &str,
    ) -> Result<Value, sqlx::Error> {
        let col = match by {
            "browser" => "browser",
            "os" => "os",
            "device" => "device",
            "country" => "country",
            _ => "browser",
        };
        // GROUP BY 1 (select ordinal) — SQLite can mis-resolve GROUP BY <alias>
        // and emit one row per event with the same displayed name.
        let sql = format!(
            "SELECT COALESCE(NULLIF(TRIM({col}), ''), 'unknown') AS name,
                    COUNT(DISTINCT COALESCE(session_id, distinct_id)) AS visitors,
                    COUNT(*) AS hits
             FROM site_events
             WHERE site_id = ? AND created_at >= ? AND created_at < ?
               AND COALESCE(bot,0)=0
             GROUP BY 1 ORDER BY visitors DESC, hits DESC LIMIT 50"
        );
        let rows = sqlx::query(&sql)
            .bind(site_id)
            .bind(since_ms)
            .bind(until_ms)
            .fetch_all(self.pool())
            .await?;
        let out: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "name": r.get::<String, _>("name"),
                    "visitors": r.get::<i64, _>("visitors"),
                    "hits": r.get::<i64, _>("hits"),
                })
            })
            .collect();
        Ok(json!({ "by": by, "rows": out }))
    }

    /// Top raw User-Agents for one site over `[since_ms, until_ms)`. Exists to
    /// audit `is_bot_ua`: a substring filter can only be tightened against the
    /// strings it is currently letting through. `hits_per_visitor` is the tell —
    /// crawlers cluster near 1-2, humans run higher.
    pub async fn get_site_user_agents(
        &self,
        site_id: &str,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Value, sqlx::Error> {
        // Includes bots so operators can audit filter misses; KPIs use bot=0.
        let rows = sqlx::query(
            "SELECT ua,
                    COUNT(DISTINCT COALESCE(session_id, distinct_id)) AS visitors,
                    COUNT(*)                                          AS hits
             FROM site_events
             WHERE site_id = ? AND created_at >= ? AND created_at < ?
               AND ua IS NOT NULL AND ua <> ''
             GROUP BY ua ORDER BY visitors DESC, hits DESC LIMIT 50",
        )
        .bind(site_id)
        .bind(since_ms)
        .bind(until_ms)
        .fetch_all(self.pool())
        .await?;
        let out: Vec<Value> = rows
            .iter()
            .map(|r| {
                let visitors = r.get::<i64, _>("visitors");
                let hits = r.get::<i64, _>("hits");
                json!({
                    "ua": r.get::<String, _>("ua"),
                    "visitors": visitors,
                    "hits": hits,
                    "hits_per_visitor": if visitors > 0 {
                        (hits as f64 / visitors as f64 * 10.0).round() / 10.0
                    } else { 0.0 },
                })
            })
            .collect();
        Ok(json!({ "rows": out }))
    }

    /// Top referrers for one site over `[since_ms, until_ms)`: unique visitors +
    /// hits per referrer, most visitors first (top 50). Skips null/empty.
    pub async fn get_site_referrers(
        &self,
        site_id: &str,
        since_ms: i64,
        until_ms: i64,
    ) -> Result<Value, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT referrer,
                    COUNT(DISTINCT COALESCE(session_id, distinct_id)) AS visitors,
                    COUNT(*)                                          AS hits
             FROM site_events
             WHERE site_id = ? AND created_at >= ? AND created_at < ?
               AND COALESCE(bot,0)=0
               AND referrer IS NOT NULL AND referrer <> ''
             GROUP BY referrer ORDER BY visitors DESC LIMIT 50",
        )
        .bind(site_id)
        .bind(since_ms)
        .bind(until_ms)
        .fetch_all(self.pool())
        .await?;
        let out: Vec<Value> = rows
            .iter()
            .map(|r| {
                json!({
                    "referrer": r.get::<String, _>("referrer"),
                    "visitors": r.get::<i64, _>("visitors"),
                    "hits": r.get::<i64, _>("hits"),
                })
            })
            .collect();
        Ok(json!({ "rows": out }))
    }
}

/// Map an issues row to the response JSON shape. Timestamps are raw epoch-ms.
fn issue_json(r: &SqliteRow) -> Value {
    let file: Option<String> = r.get("file");
    let function: Option<String> = r.get("function");
    let line: Option<String> = r.get("line");
    let resolved_at: Option<i64> = r.get("resolved_at");
    let last_version: Option<String> = r.get("last_version");
    let resolved_in_version: Option<String> = r.get("resolved_in_version");
    json!({
        "id": r.get::<i64, _>("id"),
        "fingerprint": r.get::<String, _>("fingerprint"),
        "name": r.get::<String, _>("name"),
        "file": file,
        "function": function,
        "line": line,
        "status": r.get::<String, _>("status"),
        "count": r.get::<i64, _>("count"),
        "first_seen": r.get::<i64, _>("first_seen"),
        "last_seen": r.get::<i64, _>("last_seen"),
        "last_version": last_version,
        "resolved_at": resolved_at,
        "resolved_in_version": resolved_in_version,
    })
}

// ---- free helpers ----------------------------------------------------------

/// Unix milliseconds — matches JS `Date.now()`. Duplicated from db.rs (which
/// keeps it private); kept in sync.
fn now_ms() -> i64 {
    let dur = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    i64::try_from(dur.as_millis()).unwrap_or(i64::MAX)
}

/// `now_ms()` as `f64` — the LHS of the JS `since = now - h*3600*1000`
/// computation, which is all-Number arithmetic. The epoch-ms magnitude (~1.7e12)
/// is well within f64's 2^53 integer-exact range, so no precision is lost.
fn now_ms_f() -> f64 {
    now_ms() as f64
}

/// db.js windowed-report clamp: `Number.isFinite(h) ? Math.max(lo, Math.min(hi,
/// h)) : default`. The incoming `h` has already been through parseIntQuery at the
/// route (so it's finite in practice), but this still folds a non-finite value to
/// `default` to match db.js byte-for-byte. NO floor — fractional hours are kept.
fn clamp_hours(h: f64, lo: f64, hi: f64, default: f64) -> f64 {
    if h.is_finite() {
        h.clamp(lo, hi)
    } else {
        default
    }
}

/// JS `Math.round(x*100)/100` returning a `serde_json::Value` that serializes
/// like a JS number: an INTEGER token when the rounded value is integral
/// (JS/serde both print `100`, not `100.0`), else a 2-dp float. The input is a
/// finite ratio in `[0, ...]`; NaN/inf can't occur (callers guard denominator=0).
fn js_round2(x: f64) -> Value {
    // Math.round rounds half UP (toward +inf for positives). (x*100).round() in
    // Rust rounds half away from zero, which equals JS for non-negative inputs.
    let scaled = (x * 100.0).round();
    let rounded = scaled / 100.0;
    if rounded.fract() == 0.0 {
        // Integral → emit a JSON integer so the byte output matches JS.
        json!(rounded as i64)
    } else {
        json!(rounded)
    }
}

/// Format epoch-ms as the exact string JS `new Date(ms).toISOString()` produces:
/// `YYYY-MM-DDTHH:MM:SS.mmmZ`, always UTC, always millisecond precision, year
/// zero-padded to 4 digits. Hand-rolled from the epoch (no chrono).
fn iso_ms(ms: i64) -> String {
    let (y, mo, d, h, mi, s, milli) = civil_from_ms(ms);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{milli:03}Z")
}

/// `new Date(ms).toISOString().slice(0, 10)` → `YYYY-MM-DD` (UTC day).
fn iso_date(ms: i64) -> String {
    let (y, mo, d, _, _, _, _) = civil_from_ms(ms);
    format!("{y:04}-{mo:02}-{d:02}")
}

/// ISO-week start (Monday 00:00 UTC) as `YYYY-MM-DD`, matching db.js
/// `isoWeekStart`: JS Sunday=0 shifted to Monday=1..Sunday=7, then subtract.
fn iso_week_start(ms: i64) -> String {
    let days = floor_div(ms, DAY_MS); // days since 1970-01-01 (a Thursday)
    // 1970-01-01 is a Thursday → weekday index 3 with Mon=0. day_of_week with
    // Mon=0..Sun=6: ((days + 3) mod 7).
    let dow_mon0 = floor_mod(days + 3, 7); // 0=Mon .. 6=Sun
    let monday_days = days - dow_mon0;
    let monday_ms = monday_days * DAY_MS;
    iso_date(monday_ms)
}

/// ISO-week start (Monday 00:00 UTC) as epoch-ms — the numeric form of
/// `iso_week_start`, used by the lifecycle report to bucket/compare weeks.
fn week_start_ms(ms: i64) -> i64 {
    let days = floor_div(ms, DAY_MS);
    let dow_mon0 = floor_mod(days + 3, 7); // 0=Mon .. 6=Sun
    (days - dow_mon0) * DAY_MS
}

/// Days since 1970-01-01 for a civil UTC date — Howard Hinnant's `days_from_civil`
/// (the forward of `civil_from_ms`). Used to parse a `YYYY-MM-DD` range bound.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = (y - era * 400) as i64; // [0, 399]
    let m = m as i64;
    let d = d as i64;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146_097 + doe - 719_468
}

/// Parse `YYYY-MM-DD` → UTC-midnight epoch-ms; `None` on malformed input. Ranges
/// the month/day loosely (`1..=12` / `1..=31`) — invalid combos just map to a
/// nearby valid day via the civil math, which is fine for a range bound.
pub fn parse_ymd_ms(s: &str) -> Option<i64> {
    let mut parts = s.split('-');
    let y: i64 = parts.next()?.parse().ok()?;
    let mo: u32 = parts.next()?.parse().ok()?;
    let d: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    Some(days_from_civil(y, mo, d) * DAY_MS)
}

/// Resolve the `[since_ms, until_ms)` window for `/api/stats/series`: a valid
/// `from`+`to` pair (UTC midnights; `until` = later midnight + 1 day so the end
/// day is inclusive; span clamped ≤ 366 days) takes precedence; otherwise a
/// rolling `days` window (clamped `[1,180]`, default 30) ending at end-of-today.
pub fn resolve_series_range(
    days_raw: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
) -> (i64, i64) {
    if let (Some(f), Some(t)) = (from.and_then(parse_ymd_ms), to.and_then(parse_ymd_ms)) {
        let since = f.min(t);
        let mut until = f.max(t) + DAY_MS; // inclusive end day
        let max_span = 366 * DAY_MS;
        if until - since > max_span {
            until = since + max_span;
        }
        return (since, until);
    }
    let days = clamp_hours(parse_num_query(days_raw, 30.0), 1.0, 180.0, 30.0);
    let today_midnight = (now_ms() / DAY_MS) * DAY_MS;
    let until = today_midnight + DAY_MS; // include today
    let n = days.ceil() as i64; // rows ending today inclusive
    let since = today_midnight - (n - 1) * DAY_MS;
    (since, until)
}

/// Report window `[since, until)` from dashboard picker params. `all=1` → since
/// 0 (everything). Else `from`+`to` (YYYY-MM-DD) when both parse, then rolling
/// `days`, then rolling `hours`, then `default_hours`. Shared by the range-aware
/// activation / attribution / funnel / retention report handlers.
/// (table, count expr, extra WHERE) for a dashboard series metric. Metric is
/// validated at the route, so the fallback is unreachable; it keeps it total.
fn series_source(metric: &str) -> (&'static str, &'static str, &'static str) {
    match metric {
        "dau" => (ACTIVE_INSTALLS, "COUNT(DISTINCT install_id)", ""),
        "pings" => ("pings", "COUNT(*)", ""),
        "events" => ("events", "COUNT(*)", ""),
        "errors" => ("events", "COUNT(*)", " AND name = 'error.thrown'"),
        "signups" => ("users", "COUNT(*)", ""),
        _ => ("pings", "COUNT(*)", ""),
    }
}

pub fn resolve_report_window(
    days: Option<&str>,
    from: Option<&str>,
    to: Option<&str>,
    hours: Option<&str>,
    all: bool,
    default_hours: f64,
) -> (i64, i64) {
    let now = now_ms();
    if all {
        return (0, now + DAY_MS);
    }
    if let (Some(_), Some(_)) = (from.and_then(parse_ymd_ms), to.and_then(parse_ymd_ms)) {
        return resolve_series_range(None, from, to);
    }
    if days.is_some() {
        return resolve_series_range(days, None, None);
    }
    let h = clamp_hours(parse_num_query(hours, default_hours), 1.0, 8760.0, default_hours);
    (now - (h * HOUR_MS_F) as i64, now)
}

/// Convert epoch-ms to civil (UTC) `(year, month, day, hour, min, sec, milli)`.
/// Uses Howard Hinnant's days-from-civil inverse; valid for the full i64 range.
fn civil_from_ms(ms: i64) -> (i64, u32, u32, u32, u32, u32, u32) {
    let days = floor_div(ms, DAY_MS);
    let rem_ms = floor_mod(ms, DAY_MS);
    let milli = (rem_ms % 1000) as u32;
    let secs_total = rem_ms / 1000;
    let s = (secs_total % 60) as u32;
    let mi = ((secs_total / 60) % 60) as u32;
    let h = (secs_total / 3600) as u32;

    // days -> civil (Hinnant). days is days since 1970-01-01.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = if m <= 2 { y + 1 } else { y };
    (year, m, d, h, mi, s, milli)
}

/// Floor division for i64 (rounds toward -inf), matching `Math.floor(a/b)`.
fn floor_div(a: i64, b: i64) -> i64 {
    let q = a / b;
    if (a % b != 0) && ((a < 0) != (b < 0)) {
        q - 1
    } else {
        q
    }
}

/// Non-negative modulo (result has the sign of `b`), matching the floor-div pair.
fn floor_mod(a: i64, b: i64) -> i64 {
    a - floor_div(a, b) * b
}

/// Read a loosely-typed column (a `json_extract` result that may be TEXT,
/// INTEGER, REAL, or NULL) as a `serde_json::Value`, preserving the SQLite native
/// type — exactly like JS passing `r.col` straight into the response object.
fn json_value(row: &SqliteRow, col: &str) -> Value {
    let raw = match row.try_get_raw(col) {
        Ok(r) => r,
        Err(_) => return Value::Null,
    };
    if raw.is_null() {
        return Value::Null;
    }
    match raw.type_info().name() {
        "INTEGER" => row.try_get::<i64, _>(col).map(|v| json!(v)).unwrap_or(Value::Null),
        "REAL" => row.try_get::<f64, _>(col).map(|v| json!(v)).unwrap_or(Value::Null),
        // TEXT / BLOB / anything else → string. json_extract never yields a BLOB
        // here, and our stored props values are TEXT/number/null.
        _ => row.try_get::<String, _>(col).map(Value::String).unwrap_or(Value::Null),
    }
}

/// Port of db.js `parseJsonOrNull`: parse a stored JSON TEXT column, returning
/// `null` for NULL input and the RAW string for malformed JSON (tolerant of
/// legacy rows). The three diagnostic JSON columns are written via JSON.stringify.
fn parse_json_or_null(v: Option<String>) -> Value {
    match v {
        None => Value::Null,
        Some(s) => serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_exclude_parses_trimmed_lists_case_insensitively() {
        let ex = ReportExclude::new(" a@example.com, ,B@Example.com ", "ABC-1 ,");
        assert!(ex.matches(Some("A@EXAMPLE.COM"), None));
        assert!(ex.matches(Some("b@example.com"), None));
        assert!(ex.matches(None, Some("abc-1")));
        assert!(!ex.matches(Some(""), Some("")));
        assert!(!ex.matches(Some("c@example.com"), Some("abc-2")));
        let empty = ReportExclude::new("", "");
        assert!(!empty.matches(Some("a@example.com"), Some("ABC-1")));
        assert!(!ReportExclude::default().matches(Some(""), Some("")));
    }

    // stickiness/activation_rate are RATIOS (dau/mau ∈ [0,1]), not percentages:
    // JS `Math.round(x*100)/100`. So x=1 → 1, x=1/3 → 0.33, x=2/3 → 0.67.
    #[test]
    fn js_round2_integral_is_integer_token() {
        // Integral results print WITHOUT a decimal: "1", "0", not "1.0"/"0.0".
        assert_eq!(js_round2(1.0).to_string(), "1");
        assert_eq!(js_round2(0.0).to_string(), "0");
    }

    #[test]
    fn js_round2_fractional_keeps_two_dp() {
        // round(50)/100 = 0.5.
        assert_eq!(js_round2(0.5).to_string(), "0.5");
        // 1/3 → round(33.33)/100 = 0.33.
        assert_eq!(js_round2(1.0 / 3.0).to_string(), "0.33");
        // 2/3 → round(66.66..)/100 = 0.67.
        assert_eq!(js_round2(2.0 / 3.0).to_string(), "0.67");
    }

    #[test]
    fn iso_ms_matches_js_toisostring() {
        // 2026-06-24T12:34:56.789Z — epoch ms verified against node's Date.parse.
        let ms = 1_782_304_496_789i64;
        assert_eq!(iso_ms(ms), "2026-06-24T12:34:56.789Z");
        // The unix epoch.
        assert_eq!(iso_ms(0), "1970-01-01T00:00:00.000Z");
    }

    #[test]
    fn iso_date_and_week_start() {
        // 1970-01-01 is a Thursday → its ISO week starts Monday 1969-12-29.
        assert_eq!(iso_date(0), "1970-01-01");
        assert_eq!(iso_week_start(0), "1969-12-29");
        // 2026-06-24 (Wed) → ISO week start Monday 2026-06-22.
        let wed = 1_782_304_496_789i64;
        assert_eq!(iso_date(wed), "2026-06-24");
        assert_eq!(iso_week_start(wed), "2026-06-22");
    }
}
