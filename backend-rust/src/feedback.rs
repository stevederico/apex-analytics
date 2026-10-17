//! In-app feedback — the ingest + triage half of the retired standalone
//! feedback-assistant service, folded in so feedback lands in the same DB and
//! dashboard as the analytics.
//!
//! PUBLIC (wired in `app.rs`, own rate limit + body cap):
//!   - `GET /feedback.js` — the widget a site embeds with one tag:
//!     `<script defer src="https://your-host/feedback.js" data-website-id="<write_key>">`.
//!     Same key as `/script.js`; carries no secret. Built from `widget/` and
//!     embedded from `widget/dist` (rust-embed), like the dashboard.
//!   - `GET /feedback/html2canvas-v2.js` — html2canvas-pro, lazy-loaded only when
//!     a user takes a screenshot, so it stays out of the main bundle. Immutable
//!     cache. `-v1.js` still answers (short cache) for older widget copies.
//!   - `POST /api/feedback` — one submission. `multipart/form-data` (what the
//!     widget sends: text fields + optional `file`) or JSON (no screenshot).
//!     Both are CORS "simple" requests, so the widget never preflights; OPTIONS
//!     is still answered for callers that send a custom content type. Keyed by
//!     the site's write_key in the `website` field: unknown → empty 404, like
//!     `/api/collect`. CORS reflects the caller's Origin (the collect exception).
//!
//! LIMITS (kept from feedback-assistant): message ≤ 5000 UTF-16 units → else
//! 413; screenshot JPEG/PNG only (415) and ≤ 2 MiB (413); a per-site budget of
//! `FEEDBACK_DAILY_BUDGET` (default 1000) rows per rolling 24h → 429.
//!
//! ADMIN (report router, `report_key_guard`): `GET /api/stats/feedback` (list +
//! status counts), `GET /api/feedback/{id}/screenshot` (the image bytes), and
//! `PATCH|POST /api/feedback/{id}` `{status}` (new | read | archived; POST alias
//! because Cloudflare challenges PATCH from non-browsers).

use std::collections::HashMap;

use axum::{
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use rust_embed::RustEmbed;
use serde_json::{json, Value};
use sqlx::Row;

use crate::app::AppState;
use crate::db::Db;
use crate::middleware::{visitor_country, visitor_user_agent};
use crate::validate::clamp_str;

/// Longest message, in UTF-16 code units (the widget's `textarea` counts the
/// same way, so the client and server agree on what "too long" means).
pub const MAX_MESSAGE_CHARS: usize = 5000;

/// Largest accepted screenshot (feedback-assistant's `MAX_SCREENSHOT_BYTES` default).
pub const MAX_SCREENSHOT_BYTES: usize = 2 * 1024 * 1024;

/// Route body cap: the screenshot plus room for the text fields and multipart
/// framing. A screenshot just over 2 MiB still reaches the handler and gets the
/// JSON 413; a far larger body is cut off by axum's limit (also 413).
pub const MAX_FEEDBACK_BODY_BYTES: usize = MAX_SCREENSHOT_BYTES + 64 * 1024;

const STATUSES: [&str; 3] = ["new", "read", "archived"];

pub(crate) fn err(status: StatusCode, code: &str) -> Response {
    let mut resp = Json(json!({ "error": code })).into_response();
    *resp.status_mut() = status;
    resp
}

pub(crate) fn empty_status(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .expect("static empty-body response builds")
}

pub(crate) fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn daily_budget() -> i64 {
    std::env::var("FEEDBACK_DAILY_BUDGET")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(1000)
}

/// Reflect the caller's Origin so the widget can read the response (and show
/// the server's error) from any site. Same posture as `/api/collect`.
pub(crate) fn with_cors(headers: &HeaderMap, mut resp: Response) -> Response {
    let origin = headers
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("*");
    if let Ok(val) = HeaderValue::from_str(origin) {
        resp.headers_mut().insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, val);
    }
    resp
}

// ---- widget assets -----------------------------------------------------------

#[derive(RustEmbed)]
#[folder = "widget/dist"]
struct WidgetAssets;

pub(crate) fn widget_file(name: &str, cache: &'static str) -> Response {
    match WidgetAssets::get(name) {
        Some(file) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/javascript; charset=utf-8")
            .header(header::CACHE_CONTROL, cache)
            .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
            .header("cross-origin-resource-policy", "cross-origin")
            .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
            .body(Body::from(file.data.into_owned()))
            .expect("static widget asset builds"),
        None => err(StatusCode::INTERNAL_SERVER_ERROR, "widget_not_built"),
    }
}

/// `GET /feedback.js` — short cache so a widget fix reaches sites within minutes.
pub async fn widget_js() -> Response {
    widget_file("feedback.js", "public, max-age=300, must-revalidate")
}

/// `GET /feedback/html2canvas-v2.js` — pinned html2canvas-pro build (parses
/// `oklch` and other modern colors; html2canvas 1.x threw on Tailwind v4
/// pages). The `-v2` name changes if the build ever does, so it is cached forever.
pub async fn widget_html2canvas() -> Response {
    widget_file("html2canvas-v2.js", "public, max-age=31536000, immutable")
}

/// `GET /feedback/html2canvas-v1.js` (and the legacy `/widget/html2canvas-v1.js`)
/// — older widget copies still ask for v1; serve the current build on a short
/// cache instead of a 404.
pub async fn widget_html2canvas_v1() -> Response {
    widget_file("html2canvas-v2.js", "public, max-age=3600")
}

// ---- POST /api/feedback ------------------------------------------------------

/// One parsed submission, before the site lookup.
#[derive(Default)]
pub(crate) struct Submission {
    pub(crate) website: Option<String>,
    pub(crate) message: String,
    pub(crate) url: Option<String>,
    /// Caller-reported UA (legacy `/v1` body); None → the request header.
    pub(crate) user_agent: Option<String>,
    pub(crate) app_version: Option<String>,
    pub(crate) end_user_id: Option<String>,
    pub(crate) end_user_name: Option<String>,
    pub(crate) end_user_email: Option<String>,
    /// (content type, bytes)
    pub(crate) screenshot: Option<(String, Vec<u8>)>,
}

/// CORS preflight. Only needed by callers that send a non-simple content type;
/// the widget's multipart POST skips it.
pub async fn feedback_options(headers: HeaderMap) -> Response {
    let resp = Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, "POST")
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "Content-Type")
        .header(header::ACCESS_CONTROL_MAX_AGE, "86400")
        .body(Body::empty())
        .expect("feedback OPTIONS builds");
    with_cors(&headers, resp)
}

pub async fn feedback_ingest(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let sub = if content_type.to_ascii_lowercase().starts_with("multipart/form-data") {
        match submission_from_multipart(content_type, &body) {
            Ok(s) => s,
            Err(resp) => return with_cors(&headers, resp),
        }
    } else {
        match submission_from_json(&body) {
            Ok(s) => s,
            Err(resp) => return with_cors(&headers, resp),
        }
    };

    // Unknown or missing key looks like a nonexistent route (no CORS, no body).
    let site_id = match resolve_site(&state, sub.website.as_deref()).await {
        Ok(Some(id)) => id,
        Ok(None) => return empty_status(StatusCode::NOT_FOUND),
        Err(resp) => return with_cors(&headers, resp),
    };
    match store_submission(&state, &headers, &site_id, &sub).await {
        Ok(id) => with_cors(&headers, Json(json!({ "ok": true, "id": id })).into_response()),
        Err(resp) => with_cors(&headers, resp),
    }
}

/// `FEEDBACK_LEGACY_KEYS` (`pk_a=<site id>,pk_b=<site id>`): the retired
/// feedback-assistant app keys still embedded on old pages, mapped to the site
/// each app moved to. Read per call, like the daily budget.
fn legacy_site_for(key: &str) -> Option<String> {
    let raw = std::env::var("FEEDBACK_LEGACY_KEYS").ok()?;
    raw.split(',').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k.trim() == key && !v.trim().is_empty()).then(|| v.trim().to_string())
    })
}

/// A widget key → site id. Accepts a site write_key (or id), or a legacy
/// feedback-assistant `pk_…` key listed in `FEEDBACK_LEGACY_KEYS`. Ok(None)
/// when nothing matches (callers answer an empty 404).
pub(crate) async fn resolve_site(state: &AppState, key: Option<&str>) -> Result<Option<String>, Response> {
    let Some(k) = key.filter(|k| !k.is_empty() && k.chars().count() <= 100) else {
        return Ok(None);
    };
    // A legacy key resolves through its mapped site id, which must still exist.
    let lookup = if k.starts_with("pk_") {
        match legacy_site_for(k) {
            Some(id) => id,
            None => return Ok(None),
        }
    } else {
        k.to_string()
    };
    state.db.site_by_write_key(&lookup).await.map_err(|e| {
        tracing::error!("feedback site lookup failed: {e}");
        err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
    })
}

/// Validate, budget-check and insert one submission for a resolved site.
/// Returns the new row id, or the error response (CORS not yet applied).
pub(crate) async fn store_submission(
    state: &AppState,
    headers: &HeaderMap,
    site_id: &str,
    sub: &Submission,
) -> Result<i64, Response> {
    if let Some(resp) = validate(sub) {
        return Err(resp);
    }

    let now = now_ms();
    match state.db.feedback_count_since(site_id, now - 86_400_000).await {
        Ok(n) if n >= daily_budget() => {
            let mut resp = err(StatusCode::TOO_MANY_REQUESTS, "daily_budget_exceeded");
            resp.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("3600"));
            return Err(resp);
        }
        Ok(_) => {}
        Err(e) => {
            tracing::error!("feedback budget check failed: {e}");
            return Err(err(StatusCode::INTERNAL_SERVER_ERROR, "server_error"));
        }
    }

    let ua = sub.user_agent.clone().unwrap_or_else(|| visitor_user_agent(headers));
    let row = NewFeedback {
        site_id,
        message: sub.message.trim(),
        url: clamp_str(sub.url.as_deref(), 2000),
        user_agent: clamp_str(Some(&ua), 500),
        app_version: clamp_str(sub.app_version.as_deref(), 64),
        end_user_id: clamp_str(sub.end_user_id.as_deref(), 200),
        end_user_name: clamp_str(sub.end_user_name.as_deref(), 200),
        end_user_email: clamp_str(sub.end_user_email.as_deref(), 320),
        country: visitor_country(headers),
        screenshot: sub.screenshot.as_ref().map(|(t, b)| (t.as_str(), b.as_slice())),
        status: "new",
        created_at: now,
    };
    match state.db.insert_feedback(&row).await {
        Ok(id) => {
            tracing::info!(feedback_id = id, site_id = %site_id, "feedback persisted");
            Ok(id)
        }
        Err(e) => {
            tracing::error!("feedback insert failed: {e}");
            Err(err(StatusCode::INTERNAL_SERVER_ERROR, "server_error"))
        }
    }
}

/// Size/type checks shared by both body shapes. Oversize is 413, never 400.
fn validate(sub: &Submission) -> Option<Response> {
    let message = sub.message.trim();
    if message.is_empty() {
        return Some(err(StatusCode::BAD_REQUEST, "message_required"));
    }
    if message.encode_utf16().count() > MAX_MESSAGE_CHARS {
        return Some(err(StatusCode::PAYLOAD_TOO_LARGE, "message_too_long"));
    }
    if let Some((mime, bytes)) = &sub.screenshot {
        if mime != "image/jpeg" && mime != "image/png" {
            return Some(err(StatusCode::UNSUPPORTED_MEDIA_TYPE, "screenshot_type"));
        }
        if bytes.len() > MAX_SCREENSHOT_BYTES {
            return Some(err(StatusCode::PAYLOAD_TOO_LARGE, "screenshot_too_large"));
        }
    }
    None
}

fn submission_from_json(body: &Bytes) -> Result<Submission, Response> {
    let v: Value = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(body).map_err(|_| err(StatusCode::BAD_REQUEST, "invalid_json"))?
    };
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    Ok(Submission {
        website: s("website"),
        message: s("message").unwrap_or_default(),
        url: s("url"),
        app_version: s("appVersion"),
        end_user_id: s("endUserId"),
        end_user_name: s("endUserName"),
        end_user_email: s("endUserEmail"),
        user_agent: None,
        screenshot: None,
    })
}

fn submission_from_multipart(content_type: &str, body: &[u8]) -> Result<Submission, Response> {
    let parts = parse_multipart(content_type, body)
        .ok_or_else(|| err(StatusCode::BAD_REQUEST, "invalid_multipart"))?;
    let mut sub = Submission::default();
    for p in parts {
        if p.name == "file" {
            // An empty file part is "no screenshot", not an error.
            if !p.data.is_empty() {
                sub.screenshot = Some((p.content_type, p.data.to_vec()));
            }
            continue;
        }
        let text = String::from_utf8_lossy(p.data).into_owned();
        match p.name.as_str() {
            "website" => sub.website = Some(text),
            "message" => sub.message = text,
            "url" => sub.url = Some(text),
            "appVersion" => sub.app_version = Some(text),
            "endUserId" => sub.end_user_id = Some(text),
            "endUserName" => sub.end_user_name = Some(text),
            "endUserEmail" => sub.end_user_email = Some(text),
            _ => {}
        }
    }
    Ok(sub)
}

/// One `multipart/form-data` part. `content_type` is lowercased, params dropped.
pub(crate) struct Part<'a> {
    pub(crate) name: String,
    pub(crate) content_type: String,
    pub(crate) data: &'a [u8],
}

/// Minimal RFC 7578 reader (ported from feedback-assistant): enough for a
/// browser `FormData` body, no new dependency. None on any framing error.
pub(crate) fn parse_multipart<'a>(content_type: &str, body: &'a [u8]) -> Option<Vec<Part<'a>>> {
    let boundary = content_type.split(';').skip(1).find_map(|p| {
        let (k, v) = p.trim().split_once('=')?;
        k.trim()
            .eq_ignore_ascii_case("boundary")
            .then(|| v.trim().trim_matches('"').to_string())
    })?;
    if boundary.is_empty() {
        return None;
    }
    let marker = format!("--{boundary}");
    let marker = marker.as_bytes();
    let mut parts = Vec::new();
    let mut cursor = find_bytes(body, marker)? + marker.len();
    loop {
        if body.get(cursor..cursor + 2) == Some(b"--") {
            return Some(parts);
        }
        if body.get(cursor..cursor + 2) != Some(b"\r\n") {
            return None;
        }
        let head_at = cursor + 2;
        let head_len = find_bytes(&body[head_at..], b"\r\n\r\n")?;
        let head = String::from_utf8_lossy(&body[head_at..head_at + head_len]);
        let data_at = head_at + head_len + 4;
        let next = data_at + find_bytes(&body[data_at..], marker)?;
        // The CRLF before the next boundary belongs to the delimiter.
        let data_end = if next >= data_at + 2 && &body[next - 2..next] == b"\r\n" {
            next - 2
        } else {
            next
        };
        let mut name = String::new();
        let mut ctype = String::new();
        for line in head.lines() {
            let Some((k, v)) = line.split_once(':') else { continue };
            if k.trim().eq_ignore_ascii_case("content-disposition") {
                // `filename="..."` also contains `name=`, so match the param key exactly.
                for param in v.split(';').skip(1) {
                    if let Some((pk, pv)) = param.trim().split_once('=') {
                        if pk.trim().eq_ignore_ascii_case("name") {
                            name = pv.trim().trim_matches('"').to_string();
                        }
                    }
                }
            } else if k.trim().eq_ignore_ascii_case("content-type") {
                ctype = v.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
            }
        }
        parts.push(Part { name, content_type: ctype, data: &body[data_at..data_end] });
        cursor = next + marker.len();
    }
}

fn find_bytes(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

// ---- admin: list / screenshot / status ------------------------------------------

fn query_opt<'a>(q: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    q.get(key).map(String::as_str).filter(|s| !s.is_empty())
}

/// `GET /api/stats/feedback?site=&status=&days=|from=&to=|hours=|all=1&limit=`
/// — newest first, without screenshot bytes (`has_screenshot` instead), plus
/// per-status counts over the same site + window. Window defaults to 30 days.
pub async fn stats_feedback(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let status = query_opt(&q, "status");
    if status.is_some_and(|s| !STATUSES.contains(&s)) {
        return err(StatusCode::BAD_REQUEST, "invalid_status");
    }
    let all = matches!(query_opt(&q, "all"), Some("1") | Some("true"));
    let (from, to) = crate::reports::resolve_report_window(
        query_opt(&q, "days"),
        query_opt(&q, "from"),
        query_opt(&q, "to"),
        query_opt(&q, "hours"),
        all,
        720.0,
    );
    let limit = query_opt(&q, "limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(200)
        .clamp(1, 1000);
    match state
        .db
        .list_feedback(query_opt(&q, "site"), status, from, to, limit)
        .await
    {
        Ok(v) => Json(v).into_response(),
        Err(e) => {
            tracing::error!("stats/feedback failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

fn parse_id(id: &str) -> Option<i64> {
    id.parse::<i64>().ok().filter(|n| *n >= 1)
}

/// `GET /api/feedback/{id}/screenshot` — the stored image. Only JPEG/PNG are
/// ever stored, so the type is safe to echo; `nosniff` keeps it that way.
pub async fn feedback_screenshot(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let Some(id) = parse_id(&id) else {
        return err(StatusCode::NOT_FOUND, "not_found");
    };
    match state.db.feedback_screenshot(id).await {
        Ok(Some((ctype, bytes))) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, ctype)
            .header(header::CACHE_CONTROL, "private, max-age=3600")
            .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
            .body(Body::from(bytes))
            .expect("screenshot response builds"),
        Ok(None) => err(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => {
            tracing::error!("feedback screenshot failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

/// `PATCH|POST /api/feedback/{id}` `{ "status": "new"|"read"|"archived" }`.
pub async fn patch_feedback(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let Some(id) = parse_id(&id) else {
        return err(StatusCode::NOT_FOUND, "not_found");
    };
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return err(StatusCode::BAD_REQUEST, "invalid_json"),
    };
    let status = match parsed.get("status").and_then(Value::as_str) {
        Some(s) if STATUSES.contains(&s) => s,
        _ => return err(StatusCode::BAD_REQUEST, "invalid_status"),
    };
    match state.db.set_feedback_status(id, status).await {
        Ok(true) => Json(json!({ "id": id, "status": status })).into_response(),
        Ok(false) => err(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => {
            tracing::error!("feedback status update failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

// ---- admin: import -------------------------------------------------------------

/// Most rows one import call takes.
const MAX_IMPORT_ROWS: usize = 1000;

/// One validated import row, before the duplicate check.
struct ImportRow {
    message: String,
    created_at: i64,
    url: Option<String>,
    user_agent: Option<String>,
    app_version: Option<String>,
    end_user_id: Option<String>,
    end_user_name: Option<String>,
    end_user_email: Option<String>,
    status: &'static str,
    /// (content type, bytes)
    screenshot: Option<(String, Vec<u8>)>,
}

/// `POST /api/import/feedback` — backfill from another feedback store.
///
/// Body: `{ "site": "<site id or write_key>", "dry_run": bool, "rows": [{
/// "message", "created_at" (unix ms), "url"?, "user_agent"?, "app_version"?,
/// "end_user_id"?, "end_user_name"?, "end_user_email"?, "status"?,
/// "screenshot"? ("data:image/jpeg|png;base64,…") }] }`.
///
/// Every row is validated before anything is written; one bad row rejects the
/// batch (400 + `index`), so an import is never partial. A row whose (site,
/// created_at, message) already exists is skipped, so a re-run is a no-op.
/// `dry_run` returns the same per-row plan and writes nothing.
pub async fn import_feedback(State(state): State<AppState>, body: Bytes) -> Response {
    let parsed: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return err(StatusCode::BAD_REQUEST, "invalid_json"),
    };
    let site_id = match parsed.get("site").and_then(Value::as_str) {
        Some(k) if !k.is_empty() => match state.db.site_by_write_key(k).await {
            Ok(Some(id)) => id,
            Ok(None) => return err(StatusCode::NOT_FOUND, "unknown_site"),
            Err(e) => {
                tracing::error!("feedback import site lookup failed: {e}");
                return err(StatusCode::INTERNAL_SERVER_ERROR, "server_error");
            }
        },
        _ => return err(StatusCode::BAD_REQUEST, "invalid_site"),
    };
    let dry_run = parsed.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
    let arr = match parsed.get("rows").and_then(Value::as_array) {
        Some(a) => a,
        None => return err(StatusCode::BAD_REQUEST, "invalid_rows"),
    };
    if arr.len() > MAX_IMPORT_ROWS {
        return err(StatusCode::BAD_REQUEST, "batch_too_large");
    }
    let mut rows = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        match import_row(v) {
            Ok(r) => rows.push(r),
            Err(code) => {
                let mut resp = Json(json!({ "error": code, "index": i })).into_response();
                *resp.status_mut() = StatusCode::BAD_REQUEST;
                return resp;
            }
        }
    }

    let mut plan = Vec::with_capacity(rows.len());
    let mut seen = std::collections::HashSet::new();
    let (mut imported, mut skipped) = (0i64, 0i64);
    for (i, r) in rows.iter().enumerate() {
        let exists = match state.db.feedback_exists(&site_id, r.created_at, &r.message).await {
            Ok(b) => b,
            Err(e) => {
                tracing::error!("feedback import duplicate check failed: {e}");
                return err(StatusCode::INTERNAL_SERVER_ERROR, "server_error");
            }
        };
        let duplicate = exists || !seen.insert((r.created_at, r.message.clone()));
        let mut id = Value::Null;
        if duplicate {
            skipped += 1;
        } else {
            imported += 1;
            if !dry_run {
                let row = NewFeedback {
                    site_id: &site_id,
                    message: &r.message,
                    url: r.url.clone(),
                    user_agent: r.user_agent.clone(),
                    app_version: r.app_version.clone(),
                    end_user_id: r.end_user_id.clone(),
                    end_user_name: r.end_user_name.clone(),
                    end_user_email: r.end_user_email.clone(),
                    country: None,
                    screenshot: r.screenshot.as_ref().map(|(t, b)| (t.as_str(), b.as_slice())),
                    status: r.status,
                    created_at: r.created_at,
                };
                match state.db.insert_feedback(&row).await {
                    Ok(n) => id = json!(n),
                    Err(e) => {
                        tracing::error!("feedback import insert failed: {e}");
                        return err(StatusCode::INTERNAL_SERVER_ERROR, "server_error");
                    }
                }
            }
        }
        plan.push(json!({
            "index": i,
            "action": if duplicate { "skip_duplicate" } else { "insert" },
            "id": id,
            "created_at": r.created_at,
            "status": r.status,
            "end_user_name": r.end_user_name,
            "end_user_email": r.end_user_email,
            "url": r.url,
            "message_chars": r.message.encode_utf16().count(),
            "screenshot_type": r.screenshot.as_ref().map(|(t, _)| t.clone()),
            "screenshot_bytes": r.screenshot.as_ref().map(|(_, b)| b.len()),
        }));
    }
    if !dry_run && imported > 0 {
        tracing::info!(site_id = %site_id, imported, skipped, "feedback imported");
    }
    Json(json!({
        "ok": true,
        "dry_run": dry_run,
        "site_id": site_id,
        "imported": imported,
        "skipped": skipped,
        "rows": plan,
    }))
    .into_response()
}

/// Validate one import row with the same limits as live ingest.
fn import_row(v: &Value) -> Result<ImportRow, &'static str> {
    let s = |k: &str| v.get(k).and_then(Value::as_str).filter(|x| !x.is_empty());
    let message = s("message").map(str::trim).unwrap_or("");
    if message.is_empty() {
        return Err("message_required");
    }
    if message.encode_utf16().count() > MAX_MESSAGE_CHARS {
        return Err("message_too_long");
    }
    let created_at = v
        .get("created_at")
        .and_then(Value::as_i64)
        .filter(|n| *n > 0)
        .ok_or("invalid_created_at")?;
    let status = match s("status") {
        None => "new",
        Some(x) => *STATUSES.iter().find(|st| **st == x).ok_or("invalid_status")?,
    };
    let screenshot = match s("screenshot") {
        None => None,
        Some(url) => {
            let (mime, bytes) = decode_data_url(url).ok_or("invalid_screenshot")?;
            if mime != "image/jpeg" && mime != "image/png" {
                return Err("screenshot_type");
            }
            if bytes.len() > MAX_SCREENSHOT_BYTES {
                return Err("screenshot_too_large");
            }
            Some((mime, bytes))
        }
    };
    Ok(ImportRow {
        message: message.to_string(),
        created_at,
        url: clamp_str(s("url"), 2000),
        user_agent: clamp_str(s("user_agent"), 500),
        app_version: clamp_str(s("app_version"), 64),
        end_user_id: clamp_str(s("end_user_id"), 200),
        end_user_name: clamp_str(s("end_user_name"), 200),
        end_user_email: clamp_str(s("end_user_email"), 320),
        status,
        screenshot,
    })
}

/// `data:<mime>;base64,<payload>` → (lowercased mime, bytes). None if malformed.
fn decode_data_url(url: &str) -> Option<(String, Vec<u8>)> {
    let rest = url.strip_prefix("data:")?;
    let (meta, payload) = rest.split_once(',')?;
    let mime = meta.strip_suffix(";base64")?.trim().to_ascii_lowercase();
    Some((mime, b64_decode_std(payload)?))
}

/// Standard-alphabet base64 (RFC 4648 §4), padding optional, ASCII whitespace
/// ignored. No crate: the only caller is the import path.
fn b64_decode_std(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a') as u32 + 26,
            b'0'..=b'9' => (c - b'0') as u32 + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        })
    };
    let clean: Vec<u8> = s
        .bytes()
        .filter(|c| !c.is_ascii_whitespace())
        .collect();
    let clean = clean.strip_suffix(b"==").or_else(|| clean.strip_suffix(b"=")).unwrap_or(&clean);
    if clean.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(clean.len() * 3 / 4);
    for chunk in clean.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

// ---- data access -------------------------------------------------------------

/// Inputs to `insert_feedback`, already clamped.
pub struct NewFeedback<'a> {
    pub site_id: &'a str,
    pub message: &'a str,
    pub url: Option<String>,
    pub user_agent: Option<String>,
    pub app_version: Option<String>,
    pub end_user_id: Option<String>,
    pub end_user_name: Option<String>,
    pub end_user_email: Option<String>,
    pub country: Option<String>,
    /// (content type, bytes)
    pub screenshot: Option<(&'a str, &'a [u8])>,
    /// One of `STATUSES`; live ingest always writes `new`.
    pub status: &'a str,
    pub created_at: i64,
}

impl Db {
    pub async fn insert_feedback(&self, f: &NewFeedback<'_>) -> Result<i64, sqlx::Error> {
        let id = sqlx::query(
            "INSERT INTO feedback (site_id, message, url, user_agent, app_version,
               end_user_id, end_user_name, end_user_email, country, status, created_at,
               screenshot_type, screenshot_bytes, screenshot)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(f.site_id)
        .bind(f.message)
        .bind(&f.url)
        .bind(&f.user_agent)
        .bind(&f.app_version)
        .bind(&f.end_user_id)
        .bind(&f.end_user_name)
        .bind(&f.end_user_email)
        .bind(&f.country)
        .bind(f.status)
        .bind(f.created_at)
        .bind(f.screenshot.map(|(t, _)| t))
        .bind(f.screenshot.map(|(_, b)| b.len() as i64))
        .bind(f.screenshot.map(|(_, b)| b))
        .execute(self.pool())
        .await?
        .last_insert_rowid();
        Ok(id)
    }

    /// Whether a site already has a row with this timestamp and message (the
    /// import duplicate check).
    pub async fn feedback_exists(&self, site_id: &str, created_at: i64, message: &str) -> Result<bool, sqlx::Error> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM feedback WHERE site_id = ? AND created_at = ? AND message = ?",
        )
        .bind(site_id)
        .bind(created_at)
        .bind(message)
        .fetch_one(self.pool())
        .await?;
        Ok(n > 0)
    }

    /// Rows a site has received since `since_ms` (the daily budget check).
    pub async fn feedback_count_since(&self, site_id: &str, since_ms: i64) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar("SELECT COUNT(*) FROM feedback WHERE site_id = ? AND created_at >= ?")
            .bind(site_id)
            .bind(since_ms)
            .fetch_one(self.pool())
            .await
    }

    /// `{ rows, counts: {total,new,read,archived} }`. `counts` ignore the
    /// status filter so the dashboard can show every bucket at once.
    pub async fn list_feedback(
        &self,
        site: Option<&str>,
        status: Option<&str>,
        from: i64,
        to: i64,
        limit: i64,
    ) -> Result<Value, sqlx::Error> {
        let rows = sqlx::query(
            "SELECT f.id, f.site_id, s.name AS site_name, f.message, f.url, f.user_agent,
                    f.app_version, f.end_user_id, f.end_user_name, f.end_user_email,
                    f.country, f.status, f.created_at, f.screenshot_bytes
             FROM feedback f LEFT JOIN sites s ON s.id = f.site_id
             WHERE f.created_at >= ? AND f.created_at < ?
               AND (? IS NULL OR f.site_id = ?)
               AND (? IS NULL OR f.status = ?)
             ORDER BY f.created_at DESC, f.id DESC
             LIMIT ?",
        )
        .bind(from)
        .bind(to)
        .bind(site)
        .bind(site)
        .bind(status)
        .bind(status)
        .bind(limit)
        .fetch_all(self.pool())
        .await?;
        let rows: Vec<Value> = rows
            .iter()
            .map(|r| {
                let shot: Option<i64> = r.get("screenshot_bytes");
                json!({
                    "id": r.get::<i64, _>("id"),
                    "site_id": r.get::<String, _>("site_id"),
                    "site_name": r.get::<Option<String>, _>("site_name"),
                    "message": r.get::<String, _>("message"),
                    "url": r.get::<Option<String>, _>("url"),
                    "user_agent": r.get::<Option<String>, _>("user_agent"),
                    "app_version": r.get::<Option<String>, _>("app_version"),
                    "end_user_id": r.get::<Option<String>, _>("end_user_id"),
                    "end_user_name": r.get::<Option<String>, _>("end_user_name"),
                    "end_user_email": r.get::<Option<String>, _>("end_user_email"),
                    "country": r.get::<Option<String>, _>("country"),
                    "status": r.get::<String, _>("status"),
                    "created_at": r.get::<i64, _>("created_at"),
                    "has_screenshot": shot.is_some(),
                    "screenshot_bytes": shot,
                })
            })
            .collect();
        let counts = sqlx::query(
            "SELECT COUNT(*) AS total,
                    COALESCE(SUM(status = 'new'), 0) AS new,
                    COALESCE(SUM(status = 'read'), 0) AS read,
                    COALESCE(SUM(status = 'archived'), 0) AS archived
             FROM feedback
             WHERE created_at >= ? AND created_at < ? AND (? IS NULL OR site_id = ?)",
        )
        .bind(from)
        .bind(to)
        .bind(site)
        .bind(site)
        .fetch_one(self.pool())
        .await?;
        Ok(json!({
            "rows": rows,
            "counts": {
                "total": counts.get::<i64, _>("total"),
                "new": counts.get::<i64, _>("new"),
                "read": counts.get::<i64, _>("read"),
                "archived": counts.get::<i64, _>("archived"),
            },
        }))
    }

    pub async fn feedback_screenshot(&self, id: i64) -> Result<Option<(String, Vec<u8>)>, sqlx::Error> {
        let row = sqlx::query(
            "SELECT screenshot_type, screenshot FROM feedback WHERE id = ? AND screenshot IS NOT NULL",
        )
        .bind(id)
        .fetch_optional(self.pool())
        .await?;
        Ok(row.map(|r| (r.get("screenshot_type"), r.get("screenshot"))))
    }

    pub async fn set_feedback_status(&self, id: i64, status: &str) -> Result<bool, sqlx::Error> {
        let n = sqlx::query("UPDATE feedback SET status = ? WHERE id = ?")
            .bind(status)
            .bind(id)
            .execute(self.pool())
            .await?
            .rows_affected();
        Ok(n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fields_and_file_part() {
        let body = b"--XyZ\r\nContent-Disposition: form-data; name=\"message\"\r\n\r\nhi there\r\n\
--XyZ\r\nContent-Disposition: form-data; name=\"file\"; filename=\"s.png\"\r\nContent-Type: image/png\r\n\r\n\x89PNG\r\n\x00\x01\r\n\
--XyZ--\r\n";
        let parts = parse_multipart("multipart/form-data; boundary=XyZ", body).unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].name, "message");
        assert_eq!(parts[0].data, b"hi there");
        assert_eq!(parts[1].name, "file");
        assert_eq!(parts[1].content_type, "image/png");
        // Binary data with an embedded CRLF survives intact.
        assert_eq!(parts[1].data, b"\x89PNG\r\n\x00\x01");
    }

    #[test]
    fn decodes_standard_base64_data_urls() {
        assert_eq!(b64_decode_std("aGk=").unwrap(), b"hi");
        assert_eq!(b64_decode_std("aGk").unwrap(), b"hi");
        assert_eq!(b64_decode_std("/+8=").unwrap(), [0xff, 0xef]);
        assert_eq!(b64_decode_std("aG\nk=").unwrap(), b"hi");
        assert!(b64_decode_std("a-_=").is_none());
        let (mime, bytes) = decode_data_url("data:image/JPEG;base64,/9j/").unwrap();
        assert_eq!(mime, "image/jpeg");
        assert_eq!(bytes, [0xff, 0xd8, 0xff]);
        assert!(decode_data_url("data:image/png,raw").is_none());
        assert!(decode_data_url("image/png;base64,aGk=").is_none());
    }

    #[test]
    fn rejects_bad_framing() {
        assert!(parse_multipart("multipart/form-data", b"--a\r\n").is_none());
        assert!(parse_multipart("multipart/form-data; boundary=a", b"--a\r\nno-headers").is_none());
    }
}
