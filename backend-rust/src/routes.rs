//! Real ingest/public route handlers — the A4 port of backend/server.js's
//! POST/GET handlers. The guards + rate limits are wired in `app.rs`
//! (`build_app`); this module owns the request parsing, validation, and the
//! exact response shapes/status codes/error strings server.js returns.
//!
//! BODY PARSING IS MANUAL. Every POST handler takes the raw `Bytes` and runs
//! `serde_json::from_slice` itself, so a malformed body returns the JS-parity
//! `{"error":"invalid_json"}` 400 — NOT axum's default `Json` extractor
//! rejection (which is a plain-text 4xx with a different body). The body type is
//! `serde_json::Value` so arbitrary JSON shapes parse without a fixed struct,
//! then fields are read defensively (`as_str`, `as_object`, `?? null`) exactly
//! like the JS `const { ... } = body ?? {}` destructure on a loosely-typed object.
//!
//! HEADERS: extracted via the `HeaderMap` extractor + `header_str` helper that
//! mirrors the JS `c.req.header(name)` (absent/non-UTF-8 → treated as missing).
//!
//! LANDING-VISIT ORIGIN: the route-scoped tower `CorsLayer` only *withholds*
//! ACAO for a bad origin (browser-side block); it does NOT 403. server.js
//! additionally rejects a non-matching Origin server-side with a 403. This
//! handler ports that explicit check (and the OPTIONS preflight) so a non-browser
//! caller can't bypass the origin allowlist, and sets ACAO on the success path.

use std::collections::HashMap;
use std::time::Duration;

use axum::{
    body::{Body, Bytes},
    extract::{Extension, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use rust_embed::RustEmbed;

use crate::app::AppState;
use crate::reports::ReportExclude;
use crate::middleware::{
    client_ip, mirror_trusted_env, visitor_country, visitor_user_agent, AuthedUser,
};
use crate::db::{
    Diagnostic, Event, LandingEvent, LandingVisit, Ping, RegisterUser, RejectedRegistration,
    SiteEvent,
};
use crate::validate::{
    clamp_str, email_rejection_reason, js_number, name_rejection_reason, parse_num_query,
};

// ---- small response/parse helpers ------------------------------------------

/// `c.json(value, status)` — a JSON body with an explicit status. `Json`
/// serializes the value and sets `Content-Type: application/json`; we then
/// override the status (it defaults to 200).
fn json_status(status: StatusCode, value: Value) -> Response {
    let mut resp = Json(value).into_response();
    *resp.status_mut() = status;
    resp
}

/// `{"error": <code>}` at `status` — the JS `c.json({ error }, status)` shape.
fn err(status: StatusCode, code: &str) -> Response {
    json_status(status, json!({ "error": code }))
}

/// A bare status with an EMPTY body — the JS `c.body(null, status)` shape.
fn empty_status(status: StatusCode) -> Response {
    Response::builder()
        .status(status)
        .body(Body::empty())
        .expect("static empty-body response builds")
}

/// `c.req.header(name)` as an `Option<&str>` (None when absent/non-UTF-8).
fn header_opt<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// Parse a request body as loosely-typed JSON, returning the JS-parity
/// `invalid_json` 400 on failure. An EMPTY body parses to `null` here (mirroring
/// the JS handlers, which `body ?? {}` an unparseable/empty body — but note the
/// JS `c.req.json()` THROWS on empty, so we treat truly-empty as `null` and let
/// the field reads fail validation, which yields the same observable 400 for the
/// guard/integration tests that POST empty bodies). On any parse error → Err.
fn parse_json(body: &Bytes) -> Result<Value, Response> {
    if body.is_empty() {
        // Empty body: there are no fields, so downstream validation returns the
        // relevant invalid_* 400. Represent as JSON null (≡ JS `body ?? {}`).
        return Ok(Value::Null);
    }
    serde_json::from_slice::<Value>(body).map_err(|_| err(StatusCode::BAD_REQUEST, "invalid_json"))
}

/// JS `typeof v === "string"` → `Some(&str)`, else `None`. A JSON string value
/// maps to `Some`; everything else (number/bool/object/array/null/missing) → None.
fn as_string<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// JS `v && typeof v === "object"` (and NOT an array, matching JS where arrays
/// are also `typeof "object"` — but the JS code passes them through too; here we
/// accept any JSON object OR array as "object", cloning the owned `Value`).
/// Returns the owned JSON value to store, or None when the field is
/// absent/null/non-object.
fn as_object_value(v: &Value, key: &str) -> Option<Value> {
    match v.get(key) {
        Some(inner) if inner.is_object() || inner.is_array() => Some(inner.clone()),
        _ => None,
    }
}

// ---- POST /api/register -----------------------------------------------------

/// Public bootstrap endpoint (no desktop bearer; rate 10/min). Validates
/// name → email → install_id; on each rejection logs the SPECIFIC reason to
/// `rejected_registrations` (with ip/ua/country) and returns the GENERIC
/// invalid_* 400. Success returns `{ ok, token }`.
pub async fn register(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let body = match parse_json(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let name = as_string(&body, "name");
    let email = as_string(&body, "email");
    let install_id = as_string(&body, "install_id");
    let version = as_string(&body, "version").map(str::to_string);
    let os_version = as_string(&body, "os_version").map(str::to_string);
    // Phase 1 attribution: optional landing install id forwarded by the client.
    let iid = as_string(&body, "iid")
        .filter(|s| !s.is_empty() && s.chars().count() <= 100)
        .map(str::to_string);
    // Pro free-credit device identity: SHA-256 hex of the Mac's IOPlatformUUID
    // (64 chars). Optional — old clients never send it.
    let device_id = as_string(&body, "device_id")
        .filter(|s| !s.is_empty() && s.chars().count() <= 100)
        .map(str::to_string);

    // Cloudflare CF-IPCountry: ISO-3166 alpha-2, "XX" = unknown sentinel → null.
    let cf_country = header_opt(&headers, "cf-ipcountry");
    let country = match cf_country {
        Some(c) if c != "XX" && c.chars().count() == 2 => Some(c.to_uppercase()),
        _ => None,
    };
    // ip: cf-connecting-ip, else first X-Forwarded-For segment, else null.
    let ip = header_opt(&headers, "cf-connecting-ip")
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            header_opt(&headers, "x-forwarded-for")
                .and_then(|xff| xff.split(',').next())
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        });
    let ua = header_opt(&headers, "user-agent")
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    // Helper to log a rejection with the shared field set + a specific reason.
    let log_rejection = |reason: String| {
        let r = RejectedRegistration {
            name: name.map(str::to_string),
            email: email.map(str::to_string),
            install_id: install_id.map(str::to_string),
            version: version.clone(),
            os_version: os_version.clone(),
            country: country.clone(),
            reason,
            ip: ip.clone(),
            ua: ua.clone(),
        };
        let db = state.db.clone();
        async move {
            // Best-effort audit log; a failure here must not change the client
            // response (server.js fires recordRejectedRegistration unawaited).
            let _ = db.record_rejected_registration(&r).await;
        }
    };

    // Validate name → email → install_id, in that order (server.js order).
    if let Some(reason) = name_rejection_reason(name) {
        log_rejection(reason).await;
        return err(StatusCode::BAD_REQUEST, "invalid_name");
    }
    if let Some(reason) = email_rejection_reason(email) {
        log_rejection(reason).await;
        return err(StatusCode::BAD_REQUEST, "invalid_email");
    }
    // install_id: non-empty string (JS `typeof !== "string" || length === 0`).
    let install_id = match install_id {
        Some(s) if !s.is_empty() => s,
        _ => {
            log_rejection("install_id_missing".to_string()).await;
            return err(StatusCode::BAD_REQUEST, "invalid_install_id");
        }
    };

    // Per-IP registration throttle that SURVIVES A RESTART. The in-memory rate
    // limiter in front of this route resets on every deploy, so on the one
    // endpoint that mints accounts it handed out a fresh allowance each time.
    // Counting rows instead is exact across restarts. Re-registering an existing
    // email updates its row rather than inserting, so it never trips this.
    // A returning user is never a new account: re-registering updates the row.
    // This matters — the desktop re-registers on any 404 from a guarded
    // endpoint, so throttling returning users on a shared office IP would lock
    // them out of Pro entirely.
    let email_known = matches!(
        state.db.get_user_by_email(email.unwrap_or("").trim()).await,
        Ok(Some(_))
    );
    if let (Some(ip), false) = (ip.as_deref(), email_known) {
        let since = now_ms() - 3_600_000;
        match state.db.registrations_from_ip_since(ip, since).await {
            Ok(n) if n >= register_ip_hourly_max() => {
                log_rejection(format!("ip_hourly_limit_{n}")).await;
                return err(StatusCode::TOO_MANY_REQUESTS, "too_many_registrations");
            }
            Ok(_) => {}
            // A failed count must not block a legitimate signup — the in-memory
            // limiter is still in front of this route.
            Err(e) => tracing::warn!(error = %e, "register ip throttle lookup failed"),
        }
    }

    // name/email are validated; the JS stores name.trim() and email.trim().lower().
    let name_v = name.unwrap_or("").trim().to_string();
    let email_v = email.unwrap_or("").trim().to_lowercase();
    let u = RegisterUser {
        name: name_v,
        email: email_v,
        install_id: install_id.to_string(),
        version,
        os_version,
        country,
        iid,
        device_id,
        ip: ip.clone(),
    };
    match state.db.register_user(&u).await {
        Ok(token) => {
            json_status(StatusCode::OK, json!({ "ok": true, "token": token }))
        }
        Err(e) => {
            tracing::error!("register failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

// ---- GET /api/ops/registrations -------------------------------------------

/// Admin view of registration pressure: signups grouped by IP plus the recent
/// REJECTED registrations, which have been recorded since day one
/// (`record_rejected_registration`) and surfaced nowhere. Account farming — the
/// thing that would let someone mint free Pro credit in bulk — leaves its trail
/// here. `?hours=` window (default 168), `?limit=` rows (default 100).
pub async fn ops_registrations(
    State(state): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let num = |key: &str, default: i64| -> i64 {
        q.get(key).and_then(|v| v.trim().parse::<i64>().ok()).filter(|n| *n > 0).unwrap_or(default)
    };
    let (hours, limit) = (num("hours", 168), num("limit", 100));
    match (
        state.db.signups_by_ip(hours, limit).await,
        state.db.recent_rejected_registrations(hours, limit).await,
    ) {
        (Ok(by_ip), Ok(rejected)) => json_status(
            StatusCode::OK,
            json!({
                "hours": hours,
                "ip_hourly_max": register_ip_hourly_max(),
                "signups_by_ip": by_ip,
                "rejected": rejected,
            }),
        ),
        _ => {
            tracing::error!("ops registrations query failed");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

// ---- signup → install link ---------------------------------------------------

/// Fill the bearer user's missing install link (see
/// `Db::link_install_if_unlinked`). Best-effort: a failure is logged and never
/// fails the ingest. No user extension (guard not applied) is a no-op.
async fn link_install(state: &AppState, user: Option<&Extension<AuthedUser>>, install_id: &str) {
    let Some(Extension(AuthedUser(u))) = user else { return };
    if !u.install_id.is_empty() {
        return;
    }
    if let Err(e) = state.db.link_install_if_unlinked(u.id, install_id).await {
        tracing::warn!("install link failed: {e}");
    }
}

// ---- POST /api/ping ---------------------------------------------------------

/// App-launch ping (desktop bearer; rate 60). install_id non-empty string required.
pub async fn ping(
    State(state): State<AppState>,
    user: Option<Extension<AuthedUser>>,
    body: Bytes,
) -> Response {
    let body = match parse_json(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let install_id = match as_string(&body, "install_id") {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return err(StatusCode::BAD_REQUEST, "invalid_install_id"),
    };
    let version = as_string(&body, "version").map(str::to_string);
    link_install(&state, user.as_ref(), &install_id).await;
    match state.db.record_ping(&Ping { install_id, version }).await {
        Ok(()) => json_status(StatusCode::OK, json!({ "ok": true })),
        Err(e) => {
            tracing::error!("ping failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

// ---- POST /api/events -------------------------------------------------------

/// Product event ingest (desktop bearer; rate 120). install_id non-empty, name
/// non-empty and ≤100 chars; url clamped to 500; props passed through as JSON;
/// event_id (non-empty ≤64) enables dedupe.
pub async fn events(
    State(state): State<AppState>,
    user: Option<Extension<AuthedUser>>,
    body: Bytes,
) -> Response {
    // Oversized single events used to 413 silently; keep them visible in the
    // logs so a client that starts shipping fat payloads is noticed.
    if body.len() > crate::app::MAX_BODY_BYTES {
        tracing::info!(bytes = body.len(), "events: body over the 4 KiB app-wide cap (allowed up to 32 KiB)");
    }
    let body = match parse_json(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let install_id = match as_string(&body, "install_id") {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return err(StatusCode::BAD_REQUEST, "invalid_install_id"),
    };
    let name = match as_string(&body, "name") {
        Some(s) if !s.is_empty() && s.chars().count() <= 100 => s.to_string(),
        _ => return err(StatusCode::BAD_REQUEST, "invalid_name"),
    };
    let url = as_string(&body, "url").map(|s| s.chars().take(500).collect::<String>());
    let props = as_object_value(&body, "props");
    let version = as_string(&body, "version").map(str::to_string);
    // event_id: keep only a non-empty string ≤64 chars; else None (no dedupe).
    let event_id = as_string(&body, "event_id")
        .filter(|s| !s.is_empty() && s.chars().count() <= 64)
        .map(str::to_string);

    link_install(&state, user.as_ref(), &install_id).await;
    let e = Event {
        install_id,
        name,
        url,
        props,
        version,
        event_id,
    };
    match state.db.record_event(&e).await {
        Ok(()) => json_status(StatusCode::OK, json!({ "ok": true })),
        Err(err_) => {
            tracing::error!("event failed: {err_}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

// ---- POST /api/events/batch -------------------------------------------------

/// Max events per batch request. Over this → 400 `batch_too_large`.
const MAX_BATCH: usize = 100;

/// Build an `Event` from one array element, or None when install_id/name are
/// invalid (batch drops bad items rather than failing the whole request — the
/// PostHog `/batch` behaviour). Same field reads/clamps as the single handler.
fn event_from_value(v: &Value) -> Option<Event> {
    let install_id = as_string(v, "install_id")
        .filter(|s| !s.is_empty())?
        .to_string();
    let name = as_string(v, "name")
        .filter(|s| !s.is_empty() && s.chars().count() <= 100)?
        .to_string();
    let url = as_string(v, "url").map(|s| s.chars().take(500).collect::<String>());
    let props = as_object_value(v, "props");
    let version = as_string(v, "version").map(str::to_string);
    let event_id = as_string(v, "event_id")
        .filter(|s| !s.is_empty() && s.chars().count() <= 64)
        .map(str::to_string);
    Some(Event {
        install_id,
        name,
        url,
        props,
        version,
        event_id,
    })
}

/// Batch event ingest (desktop bearer; rate 30). Accepts `{ "events": [...] }` or a
/// bare `[...]` (≤100), gzip-decompressed at the layer. Each event is validated
/// and deduped exactly like the single endpoint; invalid items are silently
/// dropped. Returns `{ ok, accepted, received }`. The desktop offline queue
/// flushes here in one round trip instead of one POST per event.
pub async fn events_batch(
    State(state): State<AppState>,
    user: Option<Extension<AuthedUser>>,
    body: Bytes,
) -> Response {
    let parsed = match parse_json(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let arr = match parsed
        .get("events")
        .and_then(Value::as_array)
        .or_else(|| parsed.as_array())
    {
        Some(a) => a,
        None => return err(StatusCode::BAD_REQUEST, "invalid_batch"),
    };
    if arr.len() > MAX_BATCH {
        return err(StatusCode::BAD_REQUEST, "batch_too_large");
    }
    let received = arr.len() as i64;
    let mut accepted = 0i64;
    let mut linked = false;
    for item in arr {
        if let Some(e) = event_from_value(item) {
            if !linked {
                link_install(&state, user.as_ref(), &e.install_id).await;
                linked = true;
            }
            match state.db.record_event(&e).await {
                Ok(()) => accepted += 1,
                Err(err_) => {
                    tracing::error!("batch event failed: {err_}");
                    return err(StatusCode::INTERNAL_SERVER_ERROR, "server_error");
                }
            }
        }
    }
    json_status(
        StatusCode::OK,
        json!({ "ok": true, "accepted": accepted, "received": received }),
    )
}

// ---- POST /api/diagnostics --------------------------------------------------

/// Redacted diagnostic bundle (desktop bearer; rate 5). install_id required;
/// fingerprint/logs/gateway pass through as JSON objects; note clamped (db layer
/// clamps to 500).
pub async fn diagnostics(State(state): State<AppState>, body: Bytes) -> Response {
    let body = match parse_json(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let install_id = match as_string(&body, "install_id") {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return err(StatusCode::BAD_REQUEST, "invalid_install_id"),
    };
    let version = as_string(&body, "version").map(str::to_string);
    let d = Diagnostic {
        install_id,
        version,
        fingerprint: as_object_value(&body, "fingerprint"),
        logs: as_object_value(&body, "logs"),
        gateway: as_object_value(&body, "gateway"),
        note: as_string(&body, "note").map(str::to_string),
    };
    match state.db.record_diagnostic(&d).await {
        Ok(()) => json_status(StatusCode::OK, json!({ "ok": true })),
        Err(e) => {
            tracing::error!("diagnostic failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

// ---- /api/landing-visit -----------------------------------------------------

/// True when `LANDING_ALLOWED_ORIGIN` is set and the request's Origin equals it
/// (both sides normalized: lowercase, no trailing `/`).
fn landing_origin_ok(state: &AppState, headers: &HeaderMap) -> bool {
    !state.landing_origin.is_empty()
        && header_opt(headers, "origin")
            .is_some_and(|o| crate::app::normalize_origin(o) == state.landing_origin)
}

/// OPTIONS preflight: 403 if Origin != allowed, else 204 with CORS headers
/// (port of the JS `app.options(...)`). The tower CorsLayer also handles
/// preflight, but this explicit handler guarantees the 403-on-wrong-origin parity.
pub async fn landing_visit_options(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Response {
    if !landing_origin_ok(&state, &headers) {
        return empty_status(StatusCode::FORBIDDEN);
    }
    Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, state.landing_origin.as_str())
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, "POST")
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "Content-Type")
        .header(header::ACCESS_CONTROL_MAX_AGE, "86400")
        .body(Body::empty())
        .expect("static OPTIONS response builds")
}

/// POST landing visit (public, no desktop bearer; rate 60). The handler ENFORCES
/// the origin server-side (403 on mismatch) and sets ACAO on success — the tower
/// CORS layer only withholds ACAO and never 403s, so this is required for parity
/// with server.js and to block non-browser callers.
pub async fn landing_visit(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !landing_origin_ok(&state, &headers) {
        return empty_status(StatusCode::FORBIDDEN);
    }

    let parsed = match parse_json(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // iid: non-empty string ≤100 chars.
    let iid = match as_string(&parsed, "iid") {
        Some(s) if !s.is_empty() && s.chars().count() <= 100 => s.to_string(),
        _ => return err(StatusCode::BAD_REQUEST, "invalid_iid"),
    };

    let ip = header_opt(&headers, "x-forwarded-for")
        .and_then(|xff| xff.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            header_opt(&headers, "x-real-ip")
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        });
    let ua = clamp_str(header_opt(&headers, "user-agent"), 500);

    let v = LandingVisit {
        iid,
        ip,
        ua,
        path: clamp_str(as_string(&parsed, "path"), 200),
        referrer: clamp_str(as_string(&parsed, "referrer"), 500),
        utm_source: clamp_str(as_string(&parsed, "utm_source"), 100),
        utm_medium: clamp_str(as_string(&parsed, "utm_medium"), 100),
        utm_campaign: clamp_str(as_string(&parsed, "utm_campaign"), 100),
        utm_term: clamp_str(as_string(&parsed, "utm_term"), 100),
        utm_content: clamp_str(as_string(&parsed, "utm_content"), 100),
    };

    match state.db.record_landing_visit(&v).await {
        Ok(()) => {
            // Set ACAO on success (the JS does `c.header(ACAO, origin)` before
            // returning the body). The tower layer also adds it, but setting it
            // here keeps the handler self-sufficient and matches server.js.
            let mut resp = json_status(StatusCode::OK, json!({ "ok": true }));
            if let Ok(val) = header::HeaderValue::from_str(&state.landing_origin) {
                resp.headers_mut()
                    .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, val);
            }
            resp
        }
        Err(e) => {
            tracing::error!("landing-visit failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

// ---- /api/landing-event -----------------------------------------------------

/// Named landing-page event ingest (the Umami mirror). Same origin/CORS/IP
/// posture as `landing_visit`: 403 on wrong Origin, ACAO on success. `iid` and
/// `name` required (non-empty, ≤100); `props` passes through as a JSON object;
/// `path` clamped to 200. OPTIONS preflight is served by `landing_visit_options`
/// (origin-checked 204 + CORS — method-agnostic, shared by both landing routes).
pub async fn landing_event(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !landing_origin_ok(&state, &headers) {
        return empty_status(StatusCode::FORBIDDEN);
    }

    let parsed = match parse_json(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    let iid = match as_string(&parsed, "iid") {
        Some(s) if !s.is_empty() && s.chars().count() <= 100 => s.to_string(),
        _ => return err(StatusCode::BAD_REQUEST, "invalid_iid"),
    };
    let name = match as_string(&parsed, "name") {
        Some(s) if !s.is_empty() && s.chars().count() <= 100 => s.to_string(),
        _ => return err(StatusCode::BAD_REQUEST, "invalid_name"),
    };

    let ip = header_opt(&headers, "x-forwarded-for")
        .and_then(|xff| xff.split(',').next())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            header_opt(&headers, "x-real-ip")
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        });
    let ua = clamp_str(header_opt(&headers, "user-agent"), 500);

    let e = LandingEvent {
        iid,
        name,
        props: as_object_value(&parsed, "props"),
        path: clamp_str(as_string(&parsed, "path"), 200),
        ip,
        ua,
    };

    match state.db.record_landing_event(&e).await {
        Ok(()) => {
            let mut resp = json_status(StatusCode::OK, json!({ "ok": true }));
            if let Ok(val) = header::HeaderValue::from_str(&state.landing_origin) {
                resp.headers_mut()
                    .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, val);
            }
            resp
        }
        Err(e) => {
            tracing::error!("landing-event failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

// ---- /api/collect + /script.js (live web-analytics ingest, Phase B) ---------

/// Milliseconds since the epoch (server clock). Used by the live collect path
/// for the session window, the daily salt bucket, and the row timestamp.
/// Accounts one IP may create per hour (`REGISTER_IP_HOURLY_MAX`, default 10 —
/// same allowance as the in-memory 10/min bucket, but measured over an hour and
/// unaffected by restarts). A non-numeric or non-positive value falls back to
/// the default rather than disabling the throttle.
fn register_ip_hourly_max() -> i64 {
    std::env::var("REGISTER_IP_HOURLY_MAX")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(10)
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Lowercase hex SHA-256 of `input`. Backs the cookieless visitor/session id.
fn sha256_hex(input: &str) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write as _;
    let digest = Sha256::digest(input.as_bytes());
    let mut s = String::with_capacity(64);
    for b in digest {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Coarse browser / os / device from a User-Agent string — deliberately a small
/// hand-rolled matcher (YAGNI: a full UA-parsing crate is overkill for the four
/// buckets the dashboard shows). Order matters: check the more specific tokens
/// first (Edg before Chrome, Chrome before Safari).
fn parse_ua(ua: &str) -> (Option<String>, Option<String>, Option<String>) {
    let u = ua.to_lowercase();
    let browser = if u.contains("edg") {
        Some("Edge")
    } else if u.contains("opr") || u.contains("opera") {
        Some("Opera")
    } else if u.contains("chrome") || u.contains("crios") {
        Some("Chrome")
    } else if u.contains("firefox") || u.contains("fxios") {
        Some("Firefox")
    } else if u.contains("safari") {
        Some("Safari")
    } else {
        None
    };
    let os = if u.contains("windows") {
        Some("Windows")
    } else if u.contains("iphone") || u.contains("ipad") || u.contains("ios") {
        Some("iOS")
    } else if u.contains("mac os") || u.contains("macintosh") {
        Some("macOS")
    } else if u.contains("android") {
        Some("Android")
    } else if u.contains("linux") {
        Some("Linux")
    } else {
        None
    };
    let device = if u.contains("mobile") || u.contains("iphone") || u.contains("android") {
        Some("mobile")
    } else if u.contains("ipad") || u.contains("tablet") {
        Some("tablet")
    } else {
        Some("desktop")
    };
    (
        browser.map(str::to_string),
        os.map(str::to_string),
        device.map(str::to_string),
    )
}

/// Known-bot / scraper User-Agent filter so crawler hits don't inflate visitors.
/// Evidence (30d top UAs on a live site, 2026-08): generic `X11; Linux` Chrome
/// farms at ~1 hit/visitor dominate Linux "visitors"; human traffic sits far higher.
/// Substring markers + CF headers + Linux desktop Chrome scraper heuristic.
pub(crate) fn is_bot_ua(ua: &str) -> bool {
    if ua.is_empty() {
        return false;
    }
    let u = ua.to_lowercase();
    const MARKERS: &[&str] = &[
        // Classic / Umami-style
        "bot", "crawl", "spider", "slurp", "bingpreview", "facebookexternalhit",
        "embedly", "quora link preview", "outbrain", "pinterest/", "developers.google.com",
        "headless", "python-requests", "curl/", "wget", "monitor", "preview",
        // AI / SEO / commercial crawlers (plain Chrome-on-Linux still needs heuristic below)
        "gptbot", "chatgpt-user", "oai-searchbot", "claudebot", "claude-web", "anthropic-ai",
        "bytespider", "petalbot", "amazonbot", "applebot", "semrush", "ahrefs", "dotbot",
        "yandex", "baiduspider", "duckduckbot", "sogou", "exabot", "facebot", "ia_archiver",
        "mj12bot", "seznambot", "ccbot", "dataforseo", "imagesift", "meta-externalagent",
        "perplexity", "youbot", "cohere-ai", "ai2bot", "diffbot", "scrapy", "httpclient",
        "go-http-client", "java/", "okhttp", "axios/", "node-fetch", "undici", "libwww",
        "http.rb", "php/", "perl", "ruby", "python-urllib", "aiohttp", "httpx",
        "selenium", "puppeteer", "playwright", "phantomjs", "cypress",
        "uptimerobot", "pingdom", "statuscake", "site24x7", "datadog", "newrelic",
        // Seen in the wild at exactly 1.0 hits/visitor with no OS/browser.
        "shap-user",
    ];
    if MARKERS.iter().any(|m| u.contains(m)) {
        return true;
    }
    // Desktop Linux Chrome farms (not Android). Real Linux browsers often use
    // Firefox or leave more fingerprint; this class runs ~1 hit/visitor while
    // human traffic sits much higher.
    is_linux_desktop_chrome_scraper(&u)
}

/// `Mozilla/5.0 (X11; Linux x86_64) … Chrome/N Safari/537.36` scraper farms.
fn is_linux_desktop_chrome_scraper(ua_lower: &str) -> bool {
    ua_lower.contains("x11; linux")
        && ua_lower.contains("chrome/")
        && !ua_lower.contains("android")
        && !ua_lower.contains("edg/")
        && !ua_lower.contains("opr/")
        && !ua_lower.contains("brave")
}

/// Cloudflare bot signals when the request came through CF (best signal).
/// `cf-verified-bot: true` or low `cf-bot-score` (0–29 = likely automated).
pub(crate) fn is_cloudflare_bot(headers: &HeaderMap) -> bool {
    if header_opt(headers, "cf-verified-bot")
        .map(|v| v.eq_ignore_ascii_case("true") || v == "1")
        .unwrap_or(false)
    {
        return true;
    }
    if let Some(score) = header_opt(headers, "cf-bot-score") {
        if let Ok(n) = score.trim().parse::<u32>() {
            // Cloudflare: 1 = automated, 99 = human. Treat low scores as bot.
            return n > 0 && n < 30;
        }
    }
    false
}

/// Combined bot decision for site ingest.
pub(crate) fn is_site_bot(ua: &str, headers: &HeaderMap) -> bool {
    (!mirror_trusted_env(headers) && is_cloudflare_bot(headers)) || is_bot_ua(ua)
}

/// Pull `utm_*` out of a URL's query string (best-effort, no external crate:
/// split on `?`, then `&`, then `=`). Returns (source, medium, campaign).
fn utm_from_url(url: &str) -> (Option<String>, Option<String>, Option<String>) {
    let Some(query) = url.split('?').nth(1) else {
        return (None, None, None);
    };
    let (mut src, mut med, mut camp) = (None, None, None);
    for pair in query.split('&') {
        let mut it = pair.splitn(2, '=');
        let (k, v) = (it.next().unwrap_or(""), it.next().unwrap_or(""));
        let val = || Some(clamp_str_owned(v, 100));
        match k {
            "utm_source" => src = val(),
            "utm_medium" => med = val(),
            "utm_campaign" => camp = val(),
            _ => {}
        }
    }
    (src, med, camp)
}

/// Truncate a `&str` to `max` chars (URL-decode not attempted — utm values are
/// already short tokens in practice). Empty → None handled by the caller.
fn clamp_str_owned(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// The vanilla tracker served at `GET /script.js`. Reads `data-website-id`
/// (= the site's write_key) and optional `data-host` off its own `<script>`
/// tag, auto-sends a pageview on load + on SPA route change, and exposes
/// Umami-shaped APIs so tag-alone / dual-fire sites keep events:
/// - `dottie.track(name, data)` (+ `track()` pageview, `track(payload)`)
/// - `dottie.identify(id|data, data?)` — sessionStorage + `identify` event;
///   subsequent payloads merge session data into `data` (no person merge)
/// - click capture: `data-umami-event` / `data-dottie-event` (+ `*-event-*` props)
/// Does **not** set `window.umami` (dual-fire must not clobber the real Umami tag).
/// Uses `sendBeacon` (falls back to keepalive `fetch` with text/plain so the
/// request stays "simple" and skips CORS preflight). Guards: bails on
/// localhost/127.0.0.1/::1 and, if `data-domains` is set, on any hostname not
/// in that comma list — so a plain tag is safe on dev without a wrapper.
/// Visitor opt-out: bails if `localStorage['dottie.disabled']==='1'` OR
/// `localStorage['umami.disabled']==='1'` (Umami-compatible kill-switch).
const TRACKER_JS: &str = r#"(function(){
  var s=document.currentScript; if(!s) return;
  var id=s.getAttribute('data-website-id'); if(!id) return;
  var host=s.getAttribute('data-host')||new URL(s.src).origin;
  var hn=location.hostname;
  if(hn==='localhost'||hn==='127.0.0.1'||hn==='::1'||hn==='') return;
  try{ var od=localStorage.getItem('dottie.disabled')==='1'||localStorage.getItem('umami.disabled')==='1'; if(od) return; }catch(e){}
  var dm=s.getAttribute('data-domains');
  if(dm&&dm.split(',').map(function(x){return x.trim();}).indexOf(hn)<0) return;
  var kId='dottie.id.'+id, kData='dottie.data.'+id;
  function ssGet(k){ try{ return sessionStorage.getItem(k); }catch(e){ return null; } }
  function ssSet(k,v){ try{ sessionStorage.setItem(k,v); }catch(e){} }
  function sess(){
    var o={}; try{ var j=ssGet(kData); if(j) o=JSON.parse(j)||{}; }catch(e){}
    var u=ssGet(kId); if(u) o.id=u; return o;
  }
  function send(type,extra){
    var p=extra||{};
    p.website=id; p.url=location.pathname+location.search; p.referrer=document.referrer;
    p.hostname=location.hostname; p.language=navigator.language;
    p.screen=(screen.width||0)+'x'+(screen.height||0);
    var sd=sess(), keys=Object.keys(sd);
    if(keys.length){ p.data=Object.assign({}, sd, p.data||{}); }
    var body=JSON.stringify({type:type,payload:p});
    try{ if(navigator.sendBeacon){ navigator.sendBeacon(host+'/api/send',body); return; } }catch(e){}
    fetch(host+'/api/send',{method:'POST',body:body,headers:{'Content-Type':'text/plain'},keepalive:true}).catch(function(){});
  }
  function track(n,d){
    if(n==null){ send('pageview'); return; }
    if(typeof n==='object'){
      if(n.name) send('event',{name:n.name,data:n.data});
      else send('pageview',n);
      return;
    }
    send('event',{name:n,data:d});
  }
  function identify(a,b){
    var uid=null, data={};
    if(typeof a==='string'){ uid=a; if(b&&typeof b==='object') data=b; }
    else if(a&&typeof a==='object') data=a;
    if(uid) ssSet(kId, uid);
    var merged=Object.assign({}, sess(), data); if(uid) merged.id=uid;
    ssSet(kData, JSON.stringify(merged));
    send('event',{name:'identify',data:merged});
  }
  document.addEventListener('click',function(e){
    var el=e.target;
    while(el&&el!==document&&el.getAttribute){
      var name=el.getAttribute('data-umami-event')||el.getAttribute('data-dottie-event');
      if(name){
        var data={}, attrs=el.attributes;
        for(var i=0;i<attrs.length;i++){
          var an=attrs[i].name, m=an.match(/^data-(?:umami|dottie)-event-(.+)$/);
          if(m) data[m[1]]=attrs[i].value;
        }
        send('event',{name:name,data:Object.keys(data).length?data:undefined});
        return;
      }
      el=el.parentNode;
    }
  },true);
  window.dottie={track:track,identify:identify};
  function view(){ send('pageview'); }
  view();
  var push=history.pushState;
  history.pushState=function(){ push.apply(this,arguments); view(); };
  addEventListener('popstate',view);
})();
"#;

/// `GET /script.js` — public, cached, `text/javascript`. The Umami-compatible
/// tracker sites embed. No auth (it's meant to be loaded by any of the 45 site
/// origins); it carries no secret itself (the write_key lives in each site's
/// `<script data-website-id>` tag, not in this file).
pub async fn script_js() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/javascript; charset=utf-8")
        .header(header::CACHE_CONTROL, "public, max-age=3600")
        .body(Body::from(TRACKER_JS))
        .expect("static script.js builds")
}

/// CORS preflight for `/api/collect`. Unlike the single-origin landing routes,
/// collect is posted from all 45 site origins, so it reflects any Origin (the
/// documented exception to "no blanket CORS", scoped to this one route + the
/// public script). The tracker's normal path is a "simple" request that skips
/// preflight; this exists only for callers that send an explicit JSON
/// content-type.
pub async fn collect_options(headers: HeaderMap) -> Response {
    let origin = header_opt(&headers, "origin").unwrap_or("*").to_string();
    let mut b = Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, "POST")
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "Content-Type")
        .header(header::ACCESS_CONTROL_MAX_AGE, "86400");
    if let Ok(val) = header::HeaderValue::from_str(&origin) {
        b = b.header(header::ACCESS_CONTROL_ALLOW_ORIGIN, val);
    }
    b.body(Body::empty()).expect("collect OPTIONS builds")
}

/// `POST /api/collect` — the live web-analytics ingest (Phase B). Public, keyed
/// by the site's `write_key` (in the body payload's `website` field), high rate
/// limit. Accepts an Umami-shaped body `{type:'pageview'|'event', payload:{...}}`
/// (also tolerates a flat body). Enriches server-side (country from CF header,
/// browser/os/device from UA), derives a cookieless visitor + session id, and
/// writes one `site_events` row. Unknown write_key → 404 (probe-resistant); bot
/// UAs are accepted (204) but dropped, matching Umami.
pub async fn collect(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let parsed = match parse_json(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };

    // The event fields live under `payload` (Umami shape); tolerate a flat body.
    let payload = parsed.get("payload").filter(|v| v.is_object()).unwrap_or(&parsed);
    let kind = as_string(&parsed, "type").unwrap_or("pageview");

    // write_key → site_id. Missing/unknown key looks like a nonexistent route.
    let write_key = match as_string(payload, "website") {
        Some(k) if !k.is_empty() && k.chars().count() <= 100 => k,
        _ => return empty_status(StatusCode::NOT_FOUND),
    };
    let site_id = match state.db.site_by_write_key(write_key).await {
        Ok(Some(id)) => id,
        Ok(None) => return empty_status(StatusCode::NOT_FOUND),
        Err(e) => {
            tracing::error!("collect site lookup failed: {e}");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "server_error");
        }
    };

    let ua_raw = visitor_user_agent(&headers);
    // Store bots with bot=1 (auditable) instead of silent drop — reports filter
    // bot=0. Client still gets 204 either way. A trusted mirror's Cloudflare
    // bot score is about the proxy hop, so it does not count.
    let bot = if is_site_bot(&ua_raw, &headers) { 1i64 } else { 0i64 };

    // Server-side enrichment — never trust the client for these.
    let country = visitor_country(&headers);
    let ip = client_ip(&headers);
    let (browser, os, device) = parse_ua(&ua_raw);

    // Cookieless identity: distinct_id = salted hash(site|ip|ua) with a daily-
    // rotating salt (server secret + day bucket). session_id continues the most
    // recent session within 30 min of inactivity, else a fresh hash.
    let now = now_ms();
    let day = now / 86_400_000;
    let salt = format!("{}:{}", state.admin_key, day);
    let distinct_id = sha256_hex(&format!("{site_id}|{ip}|{ua_raw}|{salt}"))[..32].to_string();
    let session_id = match state
        .db
        .latest_session_for(&site_id, &distinct_id, now - 30 * 60_000)
        .await
    {
        Ok(Some(sid)) => sid,
        Ok(None) => sha256_hex(&format!("{distinct_id}|{now}"))[..32].to_string(),
        Err(e) => {
            tracing::error!("collect session lookup failed: {e}");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "server_error");
        }
    };

    let url = clamp_str(as_string(payload, "url"), 500);
    let (utm_source, utm_medium, utm_campaign) = url
        .as_deref()
        .map(utm_from_url)
        .unwrap_or((None, None, None));
    let is_event = kind == "event";
    let ev = SiteEvent {
        event_type: if is_event { 2 } else { 1 },
        name: if is_event {
            clamp_str(as_string(payload, "name"), 200)
        } else {
            None
        },
        url,
        referrer: clamp_str(as_string(payload, "referrer"), 500),
        distinct_id: Some(distinct_id),
        session_id: Some(session_id),
        hostname: clamp_str(as_string(payload, "hostname"), 200),
        browser,
        os,
        device,
        country,
        utm_source,
        utm_medium,
        utm_campaign,
        props: as_object_value(payload, "data"),
        created_at: now,
        ua: clamp_str(Some(&ua_raw), 300),
        bot,
        source_event_id: None,
    };

    match state.db.insert_site_events_bulk(&site_id, &[ev]).await {
        Ok(_) => collect_ok(&headers),
        Err(e) => {
            tracing::error!("collect insert failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

/// 204 success for `/api/collect` with a reflected ACAO (so a browser that DID
/// preflight can read the response). No body — the tracker ignores it.
fn collect_ok(headers: &HeaderMap) -> Response {
    let origin = header_opt(headers, "origin").unwrap_or("*").to_string();
    let mut b = Response::builder().status(StatusCode::NO_CONTENT);
    if let Ok(val) = header::HeaderValue::from_str(&origin) {
        b = b.header(header::ACCESS_CONTROL_ALLOW_ORIGIN, val);
    }
    b.body(Body::empty()).expect("collect 204 builds")
}

// ---- GET /dashboard (+ /dashboard/*) ---------------------------------------

/// The admin dashboard is a React/Vite SPA built to `dashboard/dist` and
/// embedded into the binary at compile time via `rust-embed` — preserving the
/// "one distroless binary, no static-file directory" model (it was previously a
/// single `include_str!`'d HTML page). Vite is built with `base: "/dashboard/"`,
/// so the bundle references absolute `/dashboard/assets/...` URLs served by
/// `dashboard_asset` below. Served at `/dashboard` (NOT `/admin` — Cloudflare's
/// WAF 403-blocks `/admin` before it reaches the origin). The shell is public;
/// every byte of DATA it loads goes through the admin-key-gated `/api/stats/*`
/// endpoints (and the gRPC-Web Admin service), so serving the shell without auth
/// leaks nothing.
#[derive(RustEmbed)]
#[folder = "dashboard/dist"]
struct DashboardAssets;

/// Content-type from the file extension — kept explicit (rather than a mime
/// crate) so the dep stays feature-flag-free.
fn dashboard_content_type(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("woff2") => "font/woff2",
        Some("woff") => "font/woff",
        Some("json") => "application/json; charset=utf-8",
        Some("map") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("txt") => "text/plain; charset=utf-8",
        Some("md") => "text/markdown; charset=utf-8",
        Some("webmanifest") => "application/manifest+json",
        _ => "application/octet-stream",
    }
}

/// Serve one embedded dist file. Content-hashed `assets/*` are immutable, so
/// they get a long cache; `index.html` intentionally is not cached.
fn dashboard_file(path: &str, file: rust_embed::EmbeddedFile) -> Response {
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, dashboard_content_type(path));
    if path.starts_with("assets/") {
        builder = builder.header(header::CACHE_CONTROL, "public, max-age=31536000, immutable");
    }
    builder
        .body(Body::from(file.data.into_owned()))
        .expect("static dashboard asset builds")
}

/// The SPA entry. If `dist` was never built (a `cargo` build without the
/// frontend) this 500s loudly rather than panicking a request.
fn dashboard_index() -> Response {
    match DashboardAssets::get("index.html") {
        Some(file) => dashboard_file("index.html", file),
        None => err(StatusCode::INTERNAL_SERVER_ERROR, "dashboard_not_built"),
    }
}

/// GET /dashboard — the SPA entry (index.html).
pub async fn dashboard() -> Response {
    dashboard_index()
}

/// GET /dashboard/{*path} — a hashed asset, or the SPA shell as a fallback (the
/// UI routes by hash, so any non-asset subpath just re-serves index.html).
pub async fn dashboard_asset(Path(path): Path<String>) -> Response {
    match DashboardAssets::get(&path) {
        Some(file) => dashboard_file(&path, file),
        None => dashboard_index(),
    }
}

/// Public API docs: a single self-contained `index.html` plus its
/// icon/favicon/llms.txt, embedded from `docs/`. Static
/// and safe to serve unauthenticated — it documents the API, it doesn't call it.
#[derive(RustEmbed)]
#[folder = "docs"]
struct DocsAssets;

fn docs_index() -> Response {
    match DocsAssets::get("index.html") {
        Some(file) => dashboard_file("index.html", file),
        None => err(StatusCode::INTERNAL_SERVER_ERROR, "docs_not_built"),
    }
}

/// GET /docs — the API docs page.
pub async fn docs() -> Response {
    docs_index()
}

/// GET /docs/{*path} — an embedded docs asset (icon.png, favicon.ico, llms.txt),
/// or the docs page as a fallback.
pub async fn docs_asset(Path(path): Path<String>) -> Response {
    match DocsAssets::get(&path) {
        Some(file) => dashboard_file(&path, file),
        None => docs_index(),
    }
}

// ---- GET /api/version -------------------------------------------------------

/// Public version metadata (no auth; rate 60). Returns the AppState version
/// policy as `{ latest, minimum_required, download_url, default_provider,
/// models }`.
///
/// `models` is the same list `/api/pro/*` enforces (`ProConfig::models`, from
/// `PRO_MODELS`), so the desktop picker and the server allowlist cannot drift —
/// and adding or retiring a model is a Railway env edit, not an app update.
/// The client caches the last good response and falls back to its compiled-in
/// list when offline, so an unreachable server never leaves it with no model.
pub async fn version(State(state): State<AppState>) -> Response {
    json_status(
        StatusCode::OK,
        json!({
            "latest": state.version.latest,
            "minimum_required": state.version.min,
            "download_url": state.version.download_url,
            "default_provider": state.version.default_provider,
            "models": state.pro_models.iter()
                .map(|m| json!({ "id": m.id, "name": m.name }))
                .collect::<Vec<_>>(),
            "provider_models": state.version.provider_models.iter()
                .map(|(provider, models)| {
                    let list = models.iter()
                        .map(|m| json!({ "id": m.id, "name": m.name }))
                        .collect::<Vec<_>>();
                    (provider.clone(), json!(list))
                })
                .collect::<serde_json::Map<String, Value>>(),
            "vision_model": state.version.vision_model,
            "local_model": state.version.local_model,
            "local_model_high_mem": state.version.local_model_high_mem,
            "stt_model_url_base": state.version.stt_model_url_base,
        }),
    )
}

// ---- admin report routes (GET /api/stats/*, /api/diagnostics/*) -------------

/// Map a report function's `Result<Value, sqlx::Error>` to a 200 JSON body or a
/// `{"error":"server_error"}` 500 (the JS handlers wrap report calls in
/// try/catch and return 500 on failure — though in practice these reads never
/// throw). `what` names the report for the log line.
fn report_response(what: &str, result: Result<Value, sqlx::Error>) -> Response {
    match result {
        Ok(v) => json_status(StatusCode::OK, v),
        Err(e) => {
            tracing::error!("{what} failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

/// A `text/markdown` body — for the daily report's `?format=md` rendered output
/// so a cron/Slack step can pipe it verbatim.
fn markdown_response(body: String) -> Response {
    (
        [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
        body,
    )
        .into_response()
}

/// Render the composite daily-report JSON into the runbook's markdown shape
/// (`$BRAIN/refs/dottie-analytics.md` — Users / Activation / Top events / Errors
/// / DAU trend). Deterministic numbers only; the human/agent adds the narrative.
///
/// `stats.dau` is a rolling last-24h unique-install count. `active_7d` is UTC
/// calendar days (midnight-to-midnight). A ping at 21:31Z on D-1 counts in
/// Users DAU the next morning but not in today's trend bucket — label both.
fn render_daily_md(b: &Value, exclude: &ReportExclude) -> String {
    use std::fmt::Write as _;
    let n = |v: &Value| v.as_i64().unwrap_or(0);
    let s = &b["stats"];
    let act = &b["activation_24h"];
    let errs = &b["errors_24h"];

    let mut out = String::new();
    let _ = writeln!(out, "# Dottie Daily Report (last 24h)\n");

    let _ = writeln!(out, "## Users");
    let _ = writeln!(
        out,
        "- Total {} · MAU {} (engaged {}) · WAU {} (engaged {}) · DAU {} (rolling 24h)",
        n(&s["users"]),
        n(&s["mau"]),
        n(&s["mau_engaged"]),
        n(&s["wau"]),
        n(&s["wau_engaged"]),
        n(&s["dau"])
    );
    let _ = writeln!(
        out,
        "- Stickiness {} · pings_24h {} · events_24h {}\n",
        s["stickiness"],
        n(&s["pings_24h"]),
        n(&s["events_24h"])
    );

    // Country breakdown is already in the JSON (stats.countries all-time,
    // stats.new_users_24h_by_country) but the md body dropped it — surface a
    // top-5 all-time line + the new-installs-by-country line. Rows: {country,count}.
    let country_line = |rows: &Value| -> String {
        rows.as_array()
            .filter(|a| !a.is_empty())
            .map(|a| {
                a.iter()
                    .take(5)
                    .map(|r| format!("{} {}", r["country"].as_str().unwrap_or("?"), n(&r["count"])))
                    .collect::<Vec<_>>()
                    .join(" · ")
            })
            .unwrap_or_else(|| "none".into())
    };
    let _ = writeln!(out, "## Countries");
    let _ = writeln!(out, "- Total: {}", country_line(&s["countries"]));
    let _ = writeln!(out, "- New 24h: {}\n", country_line(&s["new_users_24h_by_country"]));

    // Person lists (new signups / last-seen active) — Claude runbook parity.
    let actv = &b["activity"];
    // Same operator-noise filter as reports::get_daily_activity (display only).
    let is_noise =
        |u: &Value| -> bool { exclude.matches(u["email"].as_str(), u["install_id"].as_str()) };
    let person_line = |u: &Value| -> String {
        format!(
            "{} <{}>",
            u["name"].as_str().unwrap_or("?"),
            u["email"].as_str().unwrap_or("?")
        )
    };
    let write_people = |out: &mut String, heading: &str, rows: Option<&Vec<Value>>, when_key: &str| {
        let _ = writeln!(out, "## {heading}");
        match rows {
            Some(a) if !a.is_empty() => {
                let _ = writeln!(out, "- count {}", a.len());
                for u in a {
                    let when = u[when_key].as_str().unwrap_or("?");
                    let ver = u["version"].as_str().unwrap_or("?");
                    let _ = writeln!(out, "- {} · {} · v{}", person_line(u), when, ver);
                }
            }
            _ => {
                let _ = writeln!(out, "- none");
            }
        }
        out.push('\n');
    };
    write_people(
        &mut out,
        "New Signups (24h)",
        actv["new_signups_24h"].as_array(),
        "signed_up_at",
    );
    write_people(
        &mut out,
        "Active Last-Seen (24h)",
        actv["active_24h"].as_array(),
        "last_seen",
    );
    if let Some(fb) = actv.get("fallback") {
        write_people(
            &mut out,
            "Signups (7d fallback)",
            fb["signups_7d"].as_array(),
            "signed_up_at",
        );
        write_people(
            &mut out,
            "Most Recent Seen (fallback)",
            fb["recent_seen"].as_array(),
            "last_seen",
        );
    }

    let _ = writeln!(out, "## Activation (24h)");
    let _ = writeln!(
        out,
        "- {}/{} activated (rate {})",
        n(&act["activated"]),
        n(&act["total"]),
        act["activation_rate"]
    );
    if let Some(users) = act["users"].as_array() {
        for u in users.iter().filter(|u| !is_noise(u)) {
            let flag = if u["activated"].as_bool().unwrap_or(false) {
                "activated"
            } else {
                "not activated"
            };
            let _ = writeln!(
                out,
                "- {} · {} · {}",
                person_line(u),
                flag,
                u["signed_up_at"].as_str().unwrap_or("?")
            );
        }
    }
    out.push('\n');

    let _ = writeln!(out, "## Top events (24h)");
    match s["top_events_24h"].as_array() {
        Some(a) if !a.is_empty() => {
            for e in a.iter().take(5) {
                let _ = writeln!(out, "- {} — {}", e["name"].as_str().unwrap_or("?"), n(&e["count"]));
            }
        }
        _ => {
            let _ = writeln!(out, "- none");
        }
    }
    out.push('\n');

    let _ = writeln!(out, "## Pro fleet");
    match b.get("pro_fleet") {
        Some(p) if p.is_object() => {
            let usd = |v: &Value| match v {
                Value::Number(n) => format!("${n}"),
                _ => "$0".to_string(),
            };
            let _ = writeln!(
                out,
                "- lifetime {}/{} req · {}/{}",
                n(&p["lifetime_requests"]),
                n(&p["lifetime_request_cap"]),
                usd(&p["lifetime_cost_usd"]),
                usd(&p["lifetime_cost_cap_usd"])
            );
            let _ = writeln!(
                out,
                "- today {}/{} req · {}/{}",
                n(&p["today_requests"]),
                n(&p["today_request_cap"]),
                usd(&p["today_cost_usd"]),
                usd(&p["today_cost_cap_usd"])
            );
            if p.get("today_dau").is_some() || p.get("today_refusals").is_some() {
                let _ = writeln!(
                    out,
                    "- today dau {} · refusals {}",
                    n(&p["today_dau"]),
                    n(&p["today_refusals"])
                );
            }
        }
        _ => {
            let _ = writeln!(out, "- unavailable");
        }
    }
    out.push('\n');

    let _ = writeln!(out, "## Refund requests (pending — approve/deny via /api/ops/pro/refunds/{{id}})");
    match b.get("refund_requests_pending") {
        None | Some(Value::Null) => {
            let _ = writeln!(out, "- unavailable");
        }
        Some(a) => match a.as_array() {
            Some(rows) if !rows.is_empty() => {
                for r in rows {
                    let _ = writeln!(
                        out,
                        "- #{} {} · sub {} · today {} req / {} tok · lifetime {} req / {} tok",
                        n(&r["id"]),
                        r["email"].as_str().unwrap_or("?"),
                        r["subscription_id"].as_str().unwrap_or("?"),
                        n(&r["requests_today"]),
                        n(&r["tokens_today"]),
                        n(&r["total_requests"]),
                        n(&r["total_tokens"])
                    );
                }
            }
            _ => {
                let _ = writeln!(out, "- none");
            }
        },
    }
    out.push('\n');

    let _ = writeln!(out, "## Errors (24h)");
    let _ = writeln!(out, "- total {} error.thrown", n(&errs["total"]));
    if let Some(a) = errs["top"].as_array() {
        for e in a.iter().take(5) {
            let _ = writeln!(
                out,
                "  - {}:{} {} — {}",
                e["file"].as_str().unwrap_or("?"),
                e["line"],
                e["function"].as_str().unwrap_or("?"),
                n(&e["count"])
            );
        }
    }
    out.push('\n');

    let _ = writeln!(out, "## Errors by version (7d)");
    match b["errors_by_version_7d"]["rows"].as_array() {
        Some(a) if !a.is_empty() => {
            for e in a.iter().take(15) {
                let _ = writeln!(
                    out,
                    "- v{} · {} — {} ({} installs)",
                    e["version"].as_str().unwrap_or("?"),
                    e["name"].as_str().unwrap_or("?"),
                    n(&e["count"]),
                    n(&e["installs"])
                );
            }
        }
        _ => {
            let _ = writeln!(out, "- none");
        }
    }
    out.push('\n');

    // Individual failure modes (code/reason/provider or file:line) — not name rollups.
    let _ = writeln!(out, "## Error signatures (7d)");
    match b["error_signatures_7d"]["rows"].as_array() {
        Some(a) if !a.is_empty() => {
            let total = n(&b["error_signatures_7d"]["total"]);
            let _ = writeln!(out, "- total {} error.* events", total);
            for e in a.iter().take(20) {
                let detail = e["detail"].as_str().unwrap_or("(no props)");
                let _ = writeln!(
                    out,
                    "- v{} · {} · {} — {} ({} installs)",
                    e["version"].as_str().unwrap_or("?"),
                    e["name"].as_str().unwrap_or("?"),
                    detail,
                    n(&e["count"]),
                    n(&e["installs"])
                );
            }
        }
        _ => {
            let _ = writeln!(out, "- none");
        }
    }
    out.push('\n');

    let _ = writeln!(out, "## New error signatures");
    match b["new_errors"]["rows"].as_array() {
        Some(a) if !a.is_empty() => {
            for e in a.iter().take(15) {
                let file = e["file"].as_str().unwrap_or("?");
                let line = &e["line"];
                let _ = writeln!(
                    out,
                    "- {} · {}:{} — {} ({} installs)",
                    e["name"].as_str().unwrap_or("?"),
                    file,
                    line,
                    n(&e["count"]),
                    n(&e["installs"])
                );
            }
        }
        _ => {
            let _ = writeln!(out, "- none");
        }
    }
    out.push('\n');

    // Growth block only exists when a repo is linked to the native app.
    if let Some(g) = b["growth"].as_object() {
        let rate = |k: &str| match g["wow"].get(k).and_then(Value::as_f64) {
            Some(r) => format!("{}{:.1}%", if r >= 0.0 { "+" } else { "" }, r),
            None => "—".to_string(),
        };
        let _ = writeln!(out, "## Growth (week over week)");
        let _ = writeln!(
            out,
            "- downloads {} ({}) · stars {} ({}) · forks {} ({})",
            n(&g["downloads"].clone()),
            rate("downloads"),
            n(&g["stars"].clone()),
            rate("stars"),
            n(&g["forks"].clone()),
            rate("forks")
        );
        out.push('\n');
    }

    let _ = writeln!(out, "## DAU trend (7d, UTC calendar days)");
    let trend: Vec<String> = b["active_7d"]["days"]
        .as_array()
        .map(|days| {
            days.iter()
                .map(|d| format!("{}:{}", d["date"].as_str().unwrap_or("?"), n(&d["dau"])))
                .collect()
        })
        .unwrap_or_default();
    let _ = writeln!(out, "- {}", trend.join(" · "));

    out
}

#[cfg(test)]
mod daily_md_tests {
    use crate::reports::ReportExclude;
    use serde_json::{json, Value};

    fn render_daily_md(b: &Value) -> String {
        super::render_daily_md(b, &ReportExclude::default())
    }

    fn section_after<'a>(md: &'a str, heading: &str) -> &'a str {
        let start = md.find(heading).unwrap_or(0);
        let rest = &md[start..];
        match rest[heading.len()..].find("\n## ") {
            Some(rel) => &rest[..heading.len() + rel],
            None => rest,
        }
    }

    #[test]
    fn labels_dau_rolling_24h_vs_utc_calendar_trend() {
        // Wang ping 2026-09-03T21:31Z: still in rolling 24h on 2026-09-04 morning,
        // but UTC calendar bucket for 2026-09-04 is empty.
        let md = render_daily_md(&json!({
            "stats": { "dau": 1, "users": 3, "mau": 2, "wau": 1 },
            "active_7d": { "days": [
                { "date": "2026-09-04", "dau": 0 },
                { "date": "2026-09-03", "dau": 1 }
            ]}
        }));
        assert!(md.contains("DAU 1 (rolling 24h)"), "{md}");
        assert!(md.contains("## DAU trend (7d, UTC calendar days)"), "{md}");
        assert!(md.contains("2026-09-04:0"), "{md}");
        assert!(md.contains("2026-09-03:1"), "{md}");
    }

    #[test]
    fn pro_fleet_section_unavailable_when_missing() {
        let md = render_daily_md(&json!({}));
        assert!(md.contains("## Pro fleet\n- unavailable\n"), "{md}");
    }

    #[test]
    fn pro_fleet_prints_dollars_from_the_pro_stats_shape() {
        let md = render_daily_md(&json!({
            "pro_fleet": {
                "lifetime_requests": 827,
                "lifetime_request_cap": 10000,
                "lifetime_cost_usd": 1.0354454,
                "lifetime_cost_cap_usd": 25,
                "today_requests": 2,
                "today_request_cap": 400,
                "today_cost_usd": 0.000952,
                "today_cost_cap_usd": 5
            }
        }));
        let section = section_after(&md, "## Pro fleet");
        assert!(section.contains("lifetime 827/10000 req · $1.0354454/$25"), "{section}");
        assert!(section.contains("today 2/400 req · $0.000952/$5"), "{section}");
    }

    #[test]
    fn pro_fleet_prints_dau_and_refusals_when_present() {
        let md = render_daily_md(&json!({
            "pro_fleet": {
                "lifetime_requests": 1,
                "lifetime_request_cap": 10,
                "lifetime_cost_usd": 0.1,
                "lifetime_cost_cap_usd": 25,
                "today_requests": 2,
                "today_request_cap": 400,
                "today_cost_usd": 0.01,
                "today_cost_cap_usd": 5,
                "today_dau": 3,
                "today_refusals": 1
            }
        }));
        let section = section_after(&md, "## Pro fleet");
        assert!(section.contains("today dau 3 · refusals 1"), "{section}");
    }

    #[test]
    fn refund_section_unavailable_when_missing() {
        let md = render_daily_md(&json!({}));
        let heading = "## Refund requests (pending — approve/deny via /api/ops/pro/refunds/{id})";
        assert!(md.contains(heading), "{md}");
        let section = section_after(&md, heading);
        assert!(section.contains("- unavailable"), "{section}");
        assert!(
            md.contains(&format!("{heading}\n- unavailable\n\n## Errors (24h)")),
            "{md}"
        );
    }

    #[test]
    fn refund_section_none_when_empty_list() {
        let md = render_daily_md(&json!({ "refund_requests_pending": [] }));
        let heading = "## Refund requests (pending — approve/deny via /api/ops/pro/refunds/{id})";
        let section = section_after(&md, heading);
        assert!(section.contains("- none"), "{section}");
    }

    #[test]
    fn refund_section_lists_pending() {
        let md = render_daily_md(&json!({
            "refund_requests_pending": [{
                "id": 7,
                "email": "wang@example.com",
                "subscription_id": "sub_x",
                "requests_today": 2,
                "tokens_today": 100,
                "total_requests": 9,
                "total_tokens": 800
            }]
        }));
        let heading = "## Refund requests (pending — approve/deny via /api/ops/pro/refunds/{id})";
        let section = section_after(&md, heading);
        assert!(section.contains("#7 wang@example.com"), "{section}");
        assert!(section.contains("sub sub_x"), "{section}");
        assert!(!section.contains("- none"), "{section}");
    }

    #[test]
    fn pro_stats_url_allowlist_blocks_exfil() {
        use super::pro_stats_url_allowed;
        let host = "pro.internal";
        assert!(pro_stats_url_allowed("http://pro.internal:8080/api/ops/pro/stats", host));
        assert!(!pro_stats_url_allowed("http://pro.internal:8080/api/ops/pro/stats", ""));
        assert!(pro_stats_url_allowed("http://127.0.0.1:9/api/ops/pro/stats", ""));
        assert!(pro_stats_url_allowed("http://localhost/api/ops/pro/stats", ""));
        assert!(!pro_stats_url_allowed("https://evil.example/steal", host));
        assert!(!pro_stats_url_allowed("http://pro.internal.evil/x", host));
        assert!(!pro_stats_url_allowed("file:///etc/passwd", host));
        assert!(!pro_stats_url_allowed("", host));
    }
}

/// Render the all-sites daily roll-up JSON into a compact markdown table
/// (totals line + one row per site, busiest first) for `?format=md`.
fn render_sites_daily_md(b: &Value) -> String {
    use std::fmt::Write as _;
    let n = |v: &Value| v.as_i64().unwrap_or(0);
    let t = &b["totals"];
    let mut out = String::new();
    let _ = writeln!(out, "# Dottie Sites Daily (last 24h)\n");
    let _ = writeln!(
        out,
        "**{} sites** · views {} · visitors {} · visits {} · events {}\n",
        n(&t["sites"]),
        n(&t["views"]),
        n(&t["visitors"]),
        n(&t["visits"]),
        n(&t["events"])
    );
    let _ = writeln!(out, "| Site | Views | Visitors | Visits | Bounce |");
    let _ = writeln!(out, "|---|--:|--:|--:|--:|");
    if let Some(a) = b["sites"].as_array() {
        for s in a {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} |",
                s["name"].as_str().unwrap_or("?"),
                n(&s["views"]),
                n(&s["visitors"]),
                n(&s["visits"]),
                s["bounce_rate"]
            );
        }
    }
    out
}

/// `c.req.query(name)` — the raw value of a query param (None when absent).
fn query_opt<'a>(q: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    q.get(key).map(String::as_str)
}

/// GET /api/stats — top-line metrics (admin; rate 30).
pub async fn stats(State(state): State<AppState>) -> Response {
    report_response("getStats", state.db.get_stats().await)
}

/// Host allowlist for the Pro stats fetch. The request carries
/// `DOTTIE_ADMIN_KEY`, so an env override to an attacker URL would exfiltrate
/// it. Loopback always passes; one more host only via `PRO_STATS_ALLOWED_HOST`
/// (`allowed_host`, exact, lowercase), so a bad URL needs two env mistakes.
pub(crate) fn pro_stats_url_allowed(url: &str, allowed_host: &str) -> bool {
    let Ok(parsed) = reqwest::Url::parse(url) else {
        return false;
    };
    let Some(host) = parsed.host_str() else {
        return false;
    };
    matches!(parsed.scheme(), "http" | "https")
        && (matches!(host, "127.0.0.1" | "localhost")
            || (!allowed_host.is_empty() && host == allowed_host))
}

fn map_pro_stats(raw: &Value) -> (Value, Value) {
    let life = &raw["lifetime"];
    let today = &raw["today"];
    (
        json!({
            "lifetime_requests": life["requests"],
            "lifetime_tokens": life["tokens"],
            "lifetime_request_cap": life["request_cap"],
            "lifetime_cost_usd": life["cost_usd"],
            "lifetime_cost_cap_usd": life["cost_cap_usd"],
            "today_requests": today["requests"],
            "today_tokens": today["tokens"],
            "today_request_cap": today["request_cap"],
            "today_cost_usd": today["cost_usd"],
            "today_cost_cap_usd": today["cost_cap_usd"],
            "today_dau": today["dau"],
            "today_refusals": today["refusals"],
        }),
        raw.get("refunds_pending").cloned().unwrap_or(json!([])),
    )
}

async fn fetch_pro_stats(url: &str, allowed_host: &str, admin_key: &str) -> Option<Value> {
    if url.is_empty() || admin_key.is_empty() || !pro_stats_url_allowed(url, allowed_host) {
        return None;
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?;
    let resp = client
        .get(url)
        .header("Authorization", format!("Bearer {admin_key}"))
        .header("User-Agent", "dottie-analytics-report/1.0")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<Value>().await.ok()
}

/// GET /api/report/daily — one-call composite of the full daily runbook so a
/// cron/Grok automation/agent hits ONE endpoint: stats, 24h errors, 7d DAU,
/// 24h activation (+ people), 24h new-signups + last-seen active (dev-filtered),
/// errors-by-version 7d, error signatures (code/reason/provider), and new error
/// signatures. `?format=md|text` renders a ready-to-send markdown body
/// (`text/markdown`); default is JSON.
/// Admin/read-gated + 30/min like the rest (`?key=` read-key path included).
/// Pro fleet + pending refunds come from dottie-pro over the private network;
/// a miss prints `- unavailable` rather than failing the whole report.
pub async fn report_daily(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let built = async {
        let (pro_fleet, refund_requests_pending) =
            match fetch_pro_stats(&state.pro_stats_url, &state.pro_stats_allowed_host, &state.admin_key)
                .await
            {
                Some(raw) => {
                    let (fleet, refunds) = map_pro_stats(&raw);
                    (fleet, refunds)
                }
                None => (Value::Null, Value::Null),
            };
        Ok::<Value, sqlx::Error>(json!({
            "window_hours": 24,
            "stats": state.db.get_stats().await?,
            "errors_24h": state.db.get_error_breakdown(Some("24")).await?,
            "active_7d": state.db.get_daily_active(7.0).await?,
            "activation_24h": state.db.get_activation_funnel(24.0).await?,
            "activity": state.db.get_daily_activity().await?,
            "errors_by_version_7d": state.db.get_errors_by_version(168.0).await?,
            "error_signatures_7d": state.db.get_error_signatures(168.0).await?,
            "new_errors": state.db.get_new_error_signatures().await?,
            "refund_requests_pending": refund_requests_pending,
            "pro_fleet": pro_fleet,
        }))
    }
    .await;
    match built {
        Ok(body) => {
            // Linked native repo (dottie-desktop-releases) growth block when present.
            let body = match github::attach_linked_growth(&state.db, "native", body).await {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!("report_daily growth attach: {e}");
                    return err(StatusCode::INTERNAL_SERVER_ERROR, "server_error");
                }
            };
            match query_opt(&q, "format") {
                Some("md") | Some("text") => {
                    markdown_response(render_daily_md(&body, state.db.report_exclude()))
                }
                _ => json_status(StatusCode::OK, body),
            }
        }
        Err(e) => {
            tracing::error!("report_daily failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

/// GET /api/report/site-daily — multi-tenant analog of `/api/report/daily`: a
/// 24h KPI roll-up across ALL web-analytics sites (one row per site + cross-site
/// totals) so a cron/agent gets every site in one call instead of looping
/// `/api/site/summary?site=` per site. `?format=md|text` renders a markdown
/// table; default JSON. Admin-gated + 30/min like the rest.
pub async fn report_site_daily(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    match state.db.get_sites_daily(24.0).await {
        Ok(mut body) => {
            if let Some(sites) = body.get_mut("sites").and_then(|v| v.as_array_mut()) {
                for site in sites.iter_mut() {
                    let id = site
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if id.is_empty() {
                        continue;
                    }
                    match github::attach_linked_growth(&state.db, &id, site.clone()).await {
                        Ok(enriched) => *site = enriched,
                        Err(e) => tracing::error!("site-daily growth attach: {e}"),
                    }
                }
            }
            match query_opt(&q, "format") {
                Some("md") | Some("text") => markdown_response(render_sites_daily_md(&body)),
                _ => json_status(StatusCode::OK, body),
            }
        }
        Err(e) => {
            tracing::error!("report_site_daily failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

/// GET /api/stats/errors — error.thrown breakdown; `hours` default 24.
pub async fn stats_errors(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    // SPECIAL: /api/stats/errors passes the RAW query value straight into
    // getErrorBreakdown (NOT via parseIntQuery). The db layer runs Number()+clamp
    // itself (missing → 24, "" → 1, "48.5" → 48.5).
    report_response(
        "getErrorBreakdown",
        state.db.get_error_breakdown(query_opt(&q, "hours")).await,
    )
}

/// Picker window from `all` / `from`+`to` / `days`. `None` when the caller sent
/// none of them (legacy `hours`/`weeks`-only path, kept for scripts + tests).
fn picker_window(q: &HashMap<String, String>) -> Option<(i64, i64)> {
    let all = matches!(query_opt(q, "all"), Some("1") | Some("true"));
    let days = query_opt(q, "days");
    let from = query_opt(q, "from");
    let to = query_opt(q, "to");
    let hours = query_opt(q, "hours");
    if all || (from.is_some() && to.is_some()) || days.is_some() || hours.is_some() {
        Some(crate::reports::resolve_report_window(days, from, to, hours, all, 24.0))
    } else {
        None
    }
}

/// GET /api/stats/retention — weekly cohort matrix. `weeks` default 8, or the
/// picker window (`days` / `from`+`to` / `all`) when supplied.
pub async fn stats_retention(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Ok(variant) = crate::variant::parse_filter(query_opt(&q, "variant")) else {
        return err(StatusCode::BAD_REQUEST, "invalid_variant");
    };
    if let Some((since, until)) = picker_window(&q) {
        return report_response(
            "getRetention",
            state.db.get_retention_window_for_variant(since, until, variant).await,
        );
    }
    let weeks = parse_num_query(query_opt(&q, "weeks"), 8.0);
    report_response("getRetention", state.db.get_retention_for_variant(weeks, variant).await)
}

/// GET /api/stats/active — daily-active sparkline; `days` default 30.
pub async fn stats_active(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let days = parse_num_query(query_opt(&q, "days"), 30.0);
    report_response("getDailyActive", state.db.get_daily_active(days).await)
}

/// GET /api/stats/series — generic per-day volume series for the dashboard.
/// `metric` ∈ {dau,events,errors,signups,pings} (required, else 400). Window is
/// `from`+`to` (YYYY-MM-DD) when both parse, else a rolling `days` (default 30).
pub async fn stats_series(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let metric = query_opt(&q, "metric").unwrap_or("");
    if !matches!(metric, "dau" | "events" | "errors" | "signups" | "pings") {
        return err(StatusCode::BAD_REQUEST, "bad_metric");
    }
    // Rolling short window → hourly buckets (the 24h view). `hours` present means
    // the picker asked for a last-N-hours window, not a calendar-day range.
    if let Some(hours) = query_opt(&q, "hours") {
        let (since, until) =
            crate::reports::resolve_report_window(None, None, None, Some(hours), false, 24.0);
        return report_response("getSeries", state.db.get_series_hourly(metric, since, until).await);
    }
    // days / from-to → daily buckets (calendar days); `all` → every-time.
    let (since, until) = if matches!(query_opt(&q, "all"), Some("1") | Some("true")) {
        crate::reports::resolve_report_window(None, None, None, None, true, 720.0)
    } else {
        crate::reports::resolve_series_range(
            query_opt(&q, "days"),
            query_opt(&q, "from"),
            query_opt(&q, "to"),
        )
    };
    report_response("getSeries", state.db.get_series(metric, since, until).await)
}

/// GET /api/stats/top-events — top event names for the window; same `days`/
/// `from`+`to` resolution as `/api/stats/series`. Feeds the Overview list so it
/// follows the header range picker.
pub async fn stats_top_events(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (since, until) = crate::reports::resolve_report_window(
        query_opt(&q, "days"),
        query_opt(&q, "from"),
        query_opt(&q, "to"),
        query_opt(&q, "hours"),
        matches!(query_opt(&q, "all"), Some("1") | Some("true")),
        24.0,
    );
    report_response("getTopEvents", state.db.get_top_events(since, until).await)
}

/// GET /api/stats/active-users — distinct active installs for the window; same
/// `days`/`hours`/`from`+`to`/`all` resolution as the other picker endpoints.
/// Feeds the Overview "Active users" KPI so it follows the header range picker.
pub async fn stats_active_users(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (since, until) = crate::reports::resolve_report_window(
        query_opt(&q, "days"),
        query_opt(&q, "from"),
        query_opt(&q, "to"),
        query_opt(&q, "hours"),
        matches!(query_opt(&q, "all"), Some("1") | Some("true")),
        24.0,
    );
    report_response("getActiveUsers", state.db.get_active_users(since, until).await)
}

/// GET /api/stats/errors-by-version — error.* by version+name; `hours` default 168.
pub async fn stats_errors_by_version(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let hours = parse_num_query(query_opt(&q, "hours"), 168.0);
    report_response("getErrorsByVersion", state.db.get_errors_by_version(hours).await)
}

/// GET /api/stats/error-signatures — individual failure modes (version + name +
/// code/reason/provider or file/function/line); `hours` default 168.
pub async fn stats_error_signatures(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let hours = parse_num_query(query_opt(&q, "hours"), 168.0);
    report_response(
        "getErrorSignatures",
        state.db.get_error_signatures(hours).await,
    )
}

/// GET /api/stats/activation — activation funnel; `hours` default 24.
/// `?variant=control|test` limits it to one experiment arm.
pub async fn stats_activation(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Ok(variant) = crate::variant::parse_filter(query_opt(&q, "variant")) else {
        return err(StatusCode::BAD_REQUEST, "invalid_variant");
    };
    let (since, until) = picker_window(&q).unwrap_or_else(default_day_window);
    report_response(
        "getActivationFunnel",
        state.db.get_activation_funnel_window_for_variant(since, until, variant).await,
    )
}

/// GET /api/stats/funnel — per-step funnel conversion + Exp 1 metrics; `hours`
/// default 24. `?variant=control|test` limits it to one experiment arm.
pub async fn stats_funnel(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Ok(variant) = crate::variant::parse_filter(query_opt(&q, "variant")) else {
        return err(StatusCode::BAD_REQUEST, "invalid_variant");
    };
    let (since, until) = picker_window(&q).unwrap_or_else(default_day_window);
    report_response(
        "getFunnelReport",
        state.db.get_funnel_report_window_for_variant(since, until, variant).await,
    )
}

/// No picker params → the last 24h, +1ms so the half-open window keeps an
/// event written this millisecond (same as `get_funnel_report(24.0)`).
fn default_day_window() -> (i64, i64) {
    let until = now_ms() + 1;
    (until - 24 * 3_600_000, until)
}

/// GET /api/stats/crashes — crash+hang grouping; `hours` default 168.
pub async fn stats_crashes(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let hours = parse_num_query(query_opt(&q, "hours"), 168.0);
    report_response("getCrashReport", state.db.get_crash_report(hours).await)
}

/// GET /api/stats/new-errors — error signatures new vs the prior week.
pub async fn stats_new_errors(State(state): State<AppState>) -> Response {
    report_response("getNewErrorSignatures", state.db.get_new_error_signatures().await)
}

/// GET /api/stats/event-details — raw props drill-down for one exact event
/// name (`name` required, 400 without it); `hours` default 720, `limit` 50.
pub async fn stats_event_details(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some(name) = query_opt(&q, "name") else {
        return err(StatusCode::BAD_REQUEST, "name_required");
    };
    let hours = parse_num_query(query_opt(&q, "hours"), 720.0);
    let limit = parse_num_query(query_opt(&q, "limit"), 50.0);
    report_response(
        "getEventDetails",
        state.db.get_event_details(name, hours, limit).await,
    )
}

// ---- Phase 1 report routes --------------------------------------------------

/// GET /api/stats/issues — error-issue list; optional `?status=` filter
/// (active | resolved | regressed). Unknown status values just match nothing.
pub async fn stats_issues(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    report_response("getIssues", state.db.get_issues(query_opt(&q, "status")).await)
}

/// PATCH /api/issues/:id — set an issue's status to `active` or `resolved`
/// (admin; rate 30). Body `{ "status": "resolved", "version": "2026.8.8" }`.
/// The optional `version` is the build the fix ships in: later sightings from
/// OLDER builds then bump count without reopening the issue. Omitted on a
/// resolve, it defaults to the configured `latest` release rather than NULL,
/// because NULL means "reopen on any sighting" and silently undoes the resolve. Returns the
/// updated issue, 404 for an unknown/non-integer id, 400 for a bad status.
pub async fn patch_issue(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    // id must be a positive integer (same numeric-guard style as diagnostics).
    let n = js_number(&id);
    let id = if n.is_finite() && n.fract() == 0.0 && n >= 1.0 && n <= i64::MAX as f64 {
        n as i64
    } else {
        return err(StatusCode::NOT_FOUND, "not_found");
    };
    let parsed = match parse_json(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let status = match as_string(&parsed, "status") {
        Some(s) if s == "active" || s == "resolved" => s,
        _ => return err(StatusCode::BAD_REQUEST, "invalid_status"),
    };
    // Default the fix version to the current release. Resolving without one
    // stores NULL, and the ingest guard treats "resolved, no version" as
    // always-regress — so the next report from ANY build, however stale,
    // reopens the issue. That is how memory_pressure, model_download_failed,
    // and conversation_engine_init_failed all flapped back to `regressed`
    // while their last sighting was weeks old on a superseded build.
    // Resolving now means "fixed in what we ship now", which is what the
    // operator meant; an explicit `version` in the body still wins.
    let explicit = as_string(&parsed, "version").filter(|v| !v.trim().is_empty());
    let version = match explicit {
        Some(v) => Some(v),
        None if status == "resolved" && !state.version.latest.trim().is_empty() => {
            Some(state.version.latest.as_str())
        }
        None => None,
    };
    match state.db.set_issue_status(id, status, version).await {
        Ok(Some(v)) => json_status(StatusCode::OK, v),
        Ok(None) => err(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => {
            tracing::error!("setIssueStatus failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

/// DELETE (or POST) /api/landing/:iid — admin purge of one visitor's landing
/// data (events + visit). POST is a Cloudflare-compatible alias (its WAF
/// challenges DELETE/PATCH from non-browser clients, like the issues route).
/// Returns `{ ok, iid, events_deleted, visits_deleted }`.
pub async fn delete_landing(
    State(state): State<AppState>,
    Path(iid): Path<String>,
) -> Response {
    match state.db.delete_landing_iid(&iid).await {
        Ok((events, visits)) => json_status(
            StatusCode::OK,
            json!({ "ok": true, "iid": iid, "events_deleted": events, "visits_deleted": visits }),
        ),
        Err(e) => {
            tracing::error!("deleteLanding failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

// ---- multi-tenant site analytics (Umami replacement) -----------------------

/// Max site events accepted per import request (batch bound, like events/batch).
const MAX_IMPORT_BATCH: usize = 2000;

/// Build a `SiteEvent` from one import array element. Missing `type` defaults to
/// 2 (custom event); missing `created_at` → 0 (caller supplies real epoch-ms).
fn site_event_from_value(v: &Value) -> SiteEvent {
    let s = |k: &str| as_string(v, k).map(str::to_string);
    let source_event_id = s("id").or_else(|| s("event_id")).and_then(|id| {
        let id: String = id.trim().chars().take(64).collect();
        if id.is_empty() { None } else { Some(id) }
    });
    SiteEvent {
        event_type: v.get("type").and_then(Value::as_i64).unwrap_or(2),
        name: s("name"),
        url: s("url"),
        referrer: s("referrer"),
        distinct_id: s("distinct_id"),
        session_id: s("session_id"),
        hostname: s("hostname"),
        browser: s("browser"),
        os: s("os"),
        device: s("device"),
        country: s("country"),
        utm_source: s("utm_source"),
        utm_medium: s("utm_medium"),
        utm_campaign: s("utm_campaign"),
        props: as_object_value(v, "props"),
        created_at: v.get("created_at").and_then(Value::as_i64).unwrap_or(0),
        // Historical Umami exports carry no User-Agent — only /api/collect does.
        ua: None,
        bot: 0,
        source_event_id,
    }
}

#[cfg(test)]
mod bot_ua_tests {
    use super::{is_bot_ua, is_linux_desktop_chrome_scraper};

    #[test]
    fn human_mac_chrome_passes() {
        let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Safari/537.36";
        assert!(!is_bot_ua(ua));
    }

    #[test]
    fn linux_x11_chrome_scraper_caught() {
        let ua = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/149.0.0.0 Safari/537.36";
        assert!(is_bot_ua(ua));
        assert!(is_linux_desktop_chrome_scraper(&ua.to_lowercase()));
    }

    #[test]
    fn android_chrome_not_scraper() {
        let ua = "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/150.0.0.0 Mobile Safari/537.36";
        assert!(!is_bot_ua(ua));
    }

    #[test]
    fn gptbot_caught() {
        assert!(is_bot_ua("Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko; compatible; GPTBot/1.0)"));
    }

    #[test]
    fn classic_bot_marker() {
        assert!(is_bot_ua("python-requests/2.28.0"));
        assert!(is_bot_ua("curl/8.0.0"));
    }

    #[test]
    fn shap_user_caught() {
        assert!(is_bot_ua(
            "Mozilla/5.0 AppleWebKit/537.36 (KHTML, like Gecko); compatible; Shap-User/0.1.0"
        ));
    }
}

/// POST /api/import/site-events — admin bulk import (historical backfill).
/// Body `{ "site": {"id","name","domain"}, "events": [ ... ] }`. Upserts the
/// site, then inserts the batch preserving each event's `created_at`.
/// An event `id` (or `event_id`) is the Umami event id. A repeat of that id,
/// or of the same site + created_at + type + session + url + name, is skipped.
/// `imported` is rows newly inserted. `skipped` is the rest of the batch.
pub async fn import_site_events(State(state): State<AppState>, body: Bytes) -> Response {
    let parsed = match parse_json(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let site = match parsed.get("site") {
        Some(s) if s.is_object() => s,
        _ => return err(StatusCode::BAD_REQUEST, "invalid_site"),
    };
    let site_id = match as_string(site, "id") {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => return err(StatusCode::BAD_REQUEST, "invalid_site_id"),
    };
    let name = as_string(site, "name")
        .map(str::to_string)
        .unwrap_or_else(|| site_id.clone());
    let domain = as_string(site, "domain").map(str::to_string);
    let arr = match parsed.get("events").and_then(Value::as_array) {
        Some(a) => a,
        None => return err(StatusCode::BAD_REQUEST, "invalid_events"),
    };
    if arr.len() > MAX_IMPORT_BATCH {
        return err(StatusCode::BAD_REQUEST, "batch_too_large");
    }
    let events: Vec<SiteEvent> = arr.iter().map(site_event_from_value).collect();
    if let Err(e) = state.db.upsert_site(&site_id, &name, domain.as_deref()).await {
        tracing::error!("upsert_site failed: {e}");
        return err(StatusCode::INTERNAL_SERVER_ERROR, "server_error");
    }
    match state.db.insert_site_events_deduped(&site_id, &events).await {
        Ok(n) => json_status(
            StatusCode::OK,
            json!({
                "ok": true,
                "imported": n,
                "skipped": events.len() as i64 - n,
            }),
        ),
        Err(e) => {
            tracing::error!("insert_site_events_deduped failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

/// POST /api/sites — create a web-analytics property.
/// Body: `{ "name", "domain"?, "id"?, "github_repo"?: "owner/name" }`.
/// Optional `github_repo` is added (if needed) and linked for the Repo tab / growth brief.
/// Returns the site row incl. `write_key` (+ `repo` / `growth` when linked).
pub async fn create_site(State(state): State<AppState>, body: Bytes) -> Response {
    let parsed = match parse_json(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let name = match as_string(&parsed, "name").map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => s.to_string(),
        None => return err(StatusCode::BAD_REQUEST, "name_required"),
    };
    let domain = as_string(&parsed, "domain")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let id = as_string(&parsed, "id")
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let github_repo = as_string(&parsed, "github_repo")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    let mut site = match state.db.create_site(&name, domain.as_deref(), id).await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("create_site failed: {e}");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "server_error");
        }
    };

    if let Some(repo) = github_repo {
        let site_id = site
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        match github::ensure_repo_linked_to_site(&state.db, &state.github, &repo, &site_id).await {
            Ok(_) => {
                match github::attach_linked_growth(&state.db, &site_id, site.clone()).await {
                    Ok(enriched) => site = enriched,
                    Err(e) => tracing::error!("create_site attach growth: {e}"),
                }
            }
            Err(e) => {
                // Site already exists — surface repo error so the UI can show it.
                return json_status(
                    StatusCode::BAD_REQUEST,
                    json!({
                        "error": "github_repo_failed",
                        "message": e,
                        "site": site,
                    }),
                );
            }
        }
    }

    json_status(StatusCode::CREATED, site)
}

/// GET /api/sites — all sites with lifetime counts (admin). Site-switcher source.
/// Linked GitHub repo (if any) appears as `repo` + latest `growth` brief.
pub async fn list_sites(State(state): State<AppState>) -> Response {
    match state.db.list_sites().await {
        Ok(mut body) => {
            if let Some(sites) = body.get_mut("sites").and_then(|v| v.as_array_mut()) {
                for site in sites.iter_mut() {
                    let id = site
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if id.is_empty() {
                        continue;
                    }
                    match github::attach_linked_growth(&state.db, &id, site.clone()).await {
                        Ok(enriched) => *site = enriched,
                        Err(e) => tracing::error!("list_sites growth attach: {e}"),
                    }
                }
            }
            json_status(StatusCode::OK, body)
        }
        Err(e) => {
            tracing::error!("listSites failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

/// GET /api/site/summary?site=&days=|from=&to= — range KPIs (visitors, visits,
/// views, bounce_rate, visit_duration_ms, events). Includes linked repo `growth`.
pub async fn site_summary(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let site = match query_opt(&q, "site") {
        Some(s) if !s.is_empty() => s,
        _ => return err(StatusCode::BAD_REQUEST, "missing_site"),
    };
    let (since, until) = site_report_window(&q);
    match state.db.get_site_summary(site, since, until).await {
        Ok(body) => match github::attach_linked_growth(&state.db, site, body).await {
            Ok(enriched) => json_status(StatusCode::OK, enriched),
            Err(e) => {
                tracing::error!("getSiteSummary growth attach: {e}");
                err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
            }
        },
        Err(e) => {
            tracing::error!("getSiteSummary failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

/// GET /api/site/series?site=&metric=pageviews|events|visitors|visits&days=|from=&to=
pub async fn site_series(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let site = match query_opt(&q, "site") {
        Some(s) if !s.is_empty() => s,
        _ => return err(StatusCode::BAD_REQUEST, "missing_site"),
    };
    let metric = query_opt(&q, "metric").unwrap_or("pageviews");
    if !matches!(metric, "pageviews" | "events" | "visitors" | "visits") {
        return err(StatusCode::BAD_REQUEST, "bad_metric");
    }
    let (since, until) = site_report_window(&q);
    report_response(
        "getSiteSeries",
        state.db.get_site_series(site, metric, since, until).await,
    )
}

/// GET /api/site/events?site=&days=|from=&to=|&hours= — event-name breakdown for one site.
pub async fn site_events(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let site = match query_opt(&q, "site") {
        Some(s) if !s.is_empty() => s,
        _ => return err(StatusCode::BAD_REQUEST, "missing_site"),
    };
    let (since, until) = site_report_window(&q);
    report_response(
        "getSiteEvents",
        state.db.get_site_events(site, since, until).await,
    )
}

/// GET /api/site/pages?site=&kind=path|url|entry|exit&days=|from=&to=|&hours=
/// Top pages (default kind=path). Window prefers days/from/to; falls back to hours.
pub async fn site_pages(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let site = match query_opt(&q, "site") {
        Some(s) if !s.is_empty() => s,
        _ => return err(StatusCode::BAD_REQUEST, "missing_site"),
    };
    let kind = query_opt(&q, "kind").unwrap_or("path");
    if !matches!(kind, "path" | "url" | "entry" | "exit") {
        return err(StatusCode::BAD_REQUEST, "bad_kind");
    }
    let (since, until) = site_report_window(&q);
    report_response(
        "getSitePages",
        state.db.get_site_pages(site, since, until, kind).await,
    )
}

/// GET /api/site/referrers?site=&days=|from=&to=|&hours= — top referrers.
pub async fn site_referrers(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let site = match query_opt(&q, "site") {
        Some(s) if !s.is_empty() => s,
        _ => return err(StatusCode::BAD_REQUEST, "missing_site"),
    };
    let (since, until) = site_report_window(&q);
    report_response(
        "getSiteReferrers",
        state.db.get_site_referrers(site, since, until).await,
    )
}

/// GET /api/site/breakdown?site=&by=browser|os|device|country&days=|from=&to=
pub async fn site_breakdown(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let site = match query_opt(&q, "site") {
        Some(s) if !s.is_empty() => s,
        _ => return err(StatusCode::BAD_REQUEST, "missing_site"),
    };
    let by = query_opt(&q, "by").unwrap_or("browser");
    if !matches!(by, "browser" | "os" | "device" | "country") {
        return err(StatusCode::BAD_REQUEST, "bad_by");
    }
    let (since, until) = site_report_window(&q);
    report_response(
        "getSiteBreakdown",
        state.db.get_site_breakdown(site, since, until, by).await,
    )
}

/// GET /api/site/user-agents?site=&days=|from=&to= — raw UAs reaching `/collect`,
/// for auditing the bot filter. Admin-gated like every other site report.
pub async fn site_user_agents(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let site = match query_opt(&q, "site") {
        Some(s) if !s.is_empty() => s,
        _ => return err(StatusCode::BAD_REQUEST, "missing_site"),
    };
    let (since, until) = site_report_window(&q);
    report_response(
        "getSiteUserAgents",
        state.db.get_site_user_agents(site, since, until).await,
    )
}

/// Picker window for site reads: `all` · `from`+`to` · `days` · rolling `hours`
/// (default 720 → 30d). Same resolution as the desktop report endpoints.
fn site_report_window(q: &HashMap<String, String>) -> (i64, i64) {
    let all = matches!(query_opt(q, "all"), Some("1") | Some("true"));
    crate::reports::resolve_report_window(
        query_opt(q, "days"),
        query_opt(q, "from"),
        query_opt(q, "to"),
        query_opt(q, "hours"),
        all,
        720.0,
    )
}

/// GET /api/stats/users — full user list with lifetime engagement; `limit`
/// default 200 (max 1000).
pub async fn stats_users(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let limit = parse_num_query(query_opt(&q, "limit"), 200.0);
    report_response("getUsersList", state.db.get_users_list(limit).await)
}

/// GET /api/stats/lifecycle — weekly new/returning/resurrecting/dormant buckets;
/// `weeks` default 12.
pub async fn stats_lifecycle(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let weeks = parse_num_query(query_opt(&q, "weeks"), 12.0);
    report_response("getLifecycle", state.db.get_lifecycle(weeks).await)
}

/// GET /api/stats/attribution — signups grouped by landing UTM; `hours`
/// default 720 (30d).
pub async fn stats_attribution(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    if let Some((since, until)) = picker_window(&q) {
        return report_response(
            "getAttribution",
            state.db.get_attribution_window(since, until).await,
        );
    }
    let hours = parse_num_query(query_opt(&q, "hours"), 720.0);
    report_response("getAttribution", state.db.get_attribution(hours).await)
}

/// GET /api/stats/breakdown — user + engaged counts grouped by one dimension;
/// `by` ∈ {version, os, country}, default version. Other values → 400.
pub async fn stats_breakdown(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let by = query_opt(&q, "by").unwrap_or("version");
    if !matches!(by, "version" | "os" | "country") {
        return err(StatusCode::BAD_REQUEST, "invalid_by");
    }
    report_response("getBreakdown", state.db.get_breakdown(by).await)
}

/// GET /api/stats/landing-events — named landing events grouped by name
/// (count + unique visitors) over the last `hours` (default 720 = 30d).
pub async fn stats_landing_events(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let hours = parse_num_query(query_opt(&q, "hours"), 720.0);
    report_response("getLandingEvents", state.db.get_landing_events(hours).await)
}

/// GET /api/stats/landing-funnel — visited → downloaded → installed → signed_up
/// distinct-iid counts over the last `hours` (default 720 = 30d).
pub async fn stats_landing_funnel(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let hours = parse_num_query(query_opt(&q, "hours"), 720.0);
    report_response("getLandingFunnel", state.db.get_landing_funnel(hours).await)
}

/// GET /api/diagnostics/list — recent diagnostic submissions wrapped in
/// `{ rows }`; `limit` default 50.
pub async fn diagnostics_list(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let limit = parse_num_query(query_opt(&q, "limit"), 50.0);
    match state.db.get_diagnostics_list(limit).await {
        Ok(rows) => json_status(StatusCode::OK, json!({ "rows": rows })),
        Err(e) => {
            tracing::error!("getDiagnosticsList failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

/// GET /api/diagnostics/:id — a single diagnostic, or `{"error":"not_found"}`
/// 404. server.js does `getDiagnostic(Number(id))`; db.js then returns null
/// unless `Number.isInteger(n)`. So `Number("12")` → 12 (lookup), `Number("12.0")`
/// → 12 (Number.isInteger true → lookup id 12), `Number("12.5")` → not an integer
/// → null → 404, `Number("nope")` → NaN → null → 404. We replicate via js_number:
/// a finite, integral, in-i64-range value is looked up; anything else is 404.
pub async fn diagnostics_detail(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let n = js_number(&id);
    let as_int = if n.is_finite() && n.fract() == 0.0 && n >= i64::MIN as f64 && n <= i64::MAX as f64
    {
        Some(n as i64)
    } else {
        None
    };
    let row = match as_int {
        Some(n) => state.db.get_diagnostic(n).await,
        None => Ok(None),
    };
    match row {
        Ok(Some(v)) => json_status(StatusCode::OK, v),
        Ok(None) => err(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => {
            tracing::error!("getDiagnostic failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

// ---- growth-chart merge (GitHub downloads / metrics / repos) ---------------

use crate::github;

/// GET /api/downloads — raw download snapshots (?repo &from &to).
pub async fn growth_downloads(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    report_response(
        "growth_downloads",
        github::query_downloads(
            &state.db,
            query_opt(&q, "repo"),
            query_opt(&q, "from"),
            query_opt(&q, "to"),
        )
        .await,
    )
}

/// GET /api/downloads/daily — day-over-day deltas (?repo).
pub async fn growth_downloads_daily(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    report_response(
        "growth_downloads_daily",
        github::downloads_daily(&state.db, query_opt(&q, "repo")).await,
    )
}

/// GET /api/downloads/wow — rolling week-over-week download growth rate (?repo).
pub async fn growth_downloads_wow(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    match query_opt(&q, "repo") {
        Some(repo) if !repo.is_empty() => report_response(
            "growth_downloads_wow",
            github::wow_series(&state.db, "downloads", Some(repo)).await,
        ),
        // No repo named → per-project rows, never a blended cross-repo total.
        _ => report_response(
            "growth_downloads_wow",
            github::wow_series_by_repo(&state.db, "downloads").await,
        ),
    }
}

/// GET /api/downloads/latest — most recent snapshot (+ per-tag totals).
pub async fn growth_downloads_latest(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    report_response(
        "growth_downloads_latest",
        github::downloads_latest(&state.db, query_opt(&q, "repo")).await,
    )
}

/// GET /api/downloads/repos — list of tracked repos (+ kinds). `?kind=website|repo|mac_app`.
pub async fn growth_downloads_repos(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let kind = query_opt(&q, "kind").and_then(|k| github::normalize_kind(Some(k)));
    report_response(
        "growth_downloads_repos",
        github::list_repo_names_json(&state.db, kind).await,
    )
}

/// POST /api/downloads/snapshot — trigger download snapshot(s).
pub async fn growth_downloads_snapshot(
    State(state): State<AppState>,
    body: Bytes,
) -> Response {
    let parsed = match parse_json(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let repo = as_string(&parsed, "repo");
    if let Some(repo) = repo {
        match github::fetch_download_snapshot(&state.db, &state.github, repo).await {
            Ok(v) => json_status(StatusCode::CREATED, v),
            Err(e) => json_status(
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({ "error": e }),
            ),
        }
    } else {
        github::fetch_all_download_snapshots(&state.db, &state.github).await;
        match github::list_repo_names(&state.db).await {
            Ok(repos) => json_status(
                StatusCode::CREATED,
                json!({ "message": "Snapshots completed for all repos", "repos": repos }),
            ),
            Err(e) => {
                tracing::error!("list repos after snapshot: {e}");
                err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
            }
        }
    }
}

/// POST /api/downloads/backfill — insert historical totals distributed by tag.
pub async fn growth_downloads_backfill(
    State(state): State<AppState>,
    body: Bytes,
) -> Response {
    let parsed = match parse_json(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let date = match as_string(&parsed, "date") {
        Some(d) if !d.is_empty() => d,
        _ => return err(StatusCode::BAD_REQUEST, "date_required"),
    };
    let total = match parsed.get("total").and_then(Value::as_i64) {
        Some(t) => t,
        None => return err(StatusCode::BAD_REQUEST, "total_required"),
    };
    let repo = match as_string(&parsed, "repo") {
        Some(r) if !r.is_empty() => r.to_string(),
        _ => match github::list_repo_names(&state.db).await {
            Ok(list) => list.into_iter().next().unwrap_or_default(),
            Err(e) => {
                tracing::error!("backfill list repos: {e}");
                return err(StatusCode::INTERNAL_SERVER_ERROR, "server_error");
            }
        },
    };
    if repo.is_empty() {
        return err(StatusCode::BAD_REQUEST, "repo_required");
    }
    match github::backfill_downloads(&state.db, &repo, date, total).await {
        Ok(v) => json_status(StatusCode::CREATED, v),
        Err(e) => json_status(StatusCode::BAD_REQUEST, json!({ "error": e })),
    }
}

/// GET /api/metrics?metric=stars|forks|clones|views
pub async fn growth_metrics(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some(metric) = query_opt(&q, "metric") else {
        return err(StatusCode::BAD_REQUEST, "metric_required");
    };
    if !github::VALID_METRICS.contains(&metric) {
        return err(StatusCode::BAD_REQUEST, "invalid_metric");
    }
    report_response(
        "growth_metrics",
        github::query_metrics(
            &state.db,
            metric,
            query_opt(&q, "repo"),
            query_opt(&q, "from"),
            query_opt(&q, "to"),
        )
        .await,
    )
}

/// GET /api/metrics/daily?metric=
pub async fn growth_metrics_daily(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some(metric) = query_opt(&q, "metric") else {
        return err(StatusCode::BAD_REQUEST, "metric_required");
    };
    if !github::VALID_METRICS.contains(&metric) {
        return err(StatusCode::BAD_REQUEST, "invalid_metric");
    }
    report_response(
        "growth_metrics_daily",
        github::metrics_daily(&state.db, metric, query_opt(&q, "repo")).await,
    )
}

/// GET /api/metrics/latest?metric=
pub async fn growth_metrics_latest(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some(metric) = query_opt(&q, "metric") else {
        return err(StatusCode::BAD_REQUEST, "metric_required");
    };
    if !github::VALID_METRICS.contains(&metric) {
        return err(StatusCode::BAD_REQUEST, "invalid_metric");
    }
    report_response(
        "growth_metrics_latest",
        github::metrics_latest(&state.db, metric, query_opt(&q, "repo")).await,
    )
}

/// GET /api/metrics/wow?metric= — rolling week-over-week growth rate (?repo).
/// Accepts `downloads` too, so one URL shape serves every growth metric.
pub async fn growth_metrics_wow(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some(metric) = query_opt(&q, "metric") else {
        return err(StatusCode::BAD_REQUEST, "metric_required");
    };
    if !github::VALID_RATE_METRICS.contains(&metric) {
        return err(StatusCode::BAD_REQUEST, "invalid_metric");
    }
    match query_opt(&q, "repo") {
        Some(repo) if !repo.is_empty() => report_response(
            "growth_metrics_wow",
            github::wow_series(&state.db, metric, Some(repo)).await,
        ),
        // No repo named → per-project rows, never a blended cross-repo total.
        _ => report_response(
            "growth_metrics_wow",
            github::wow_series_by_repo(&state.db, metric).await,
        ),
    }
}

/// POST /api/metrics/snapshot
pub async fn growth_metrics_snapshot(
    State(state): State<AppState>,
    body: Bytes,
) -> Response {
    let parsed = match parse_json(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let metric = as_string(&parsed, "metric");
    if let Some(m) = metric {
        if !github::VALID_METRICS.contains(&m) {
            return err(StatusCode::BAD_REQUEST, "invalid_metric");
        }
    }
    let repo = as_string(&parsed, "repo").map(str::to_string);
    let repos = match &repo {
        Some(r) => vec![r.clone()],
        None => match github::list_repo_names(&state.db).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("metrics snapshot list: {e}");
                return err(StatusCode::INTERNAL_SERVER_ERROR, "server_error");
            }
        },
    };
    let metrics: Vec<&str> = match metric {
        Some(m) => vec![m],
        None => github::VALID_METRICS.to_vec(),
    };
    for r in &repos {
        for m in &metrics {
            if let Err(e) = github::fetch_metric(&state.db, &state.github, r, m).await {
                tracing::error!(repo = %r, metric = %m, error = %e, "metric snapshot failed");
            }
        }
    }
    json_status(
        StatusCode::CREATED,
        json!({ "message": "Metric snapshots completed", "repos": repos, "metrics": metrics }),
    )
}

/// GET /api/repos — full repo rows (`?kind=` optional filter).
pub async fn growth_list_repos(
    State(state): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let kind = query_opt(&q, "kind").and_then(|k| github::normalize_kind(Some(k)));
    report_response("growth_list_repos", github::list_repos(&state.db, kind).await)
}

/// POST /api/repos — add owner/name after GitHub validation. Body `{repo, kind?}`.
pub async fn growth_add_repo(State(state): State<AppState>, body: Bytes) -> Response {
    let parsed = match parse_json(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let repo = match as_string(&parsed, "repo") {
        Some(r) => r,
        None => return err(StatusCode::BAD_REQUEST, "repo_required"),
    };
    let kind = as_string(&parsed, "kind");
    match github::add_repo(&state.db, &state.github, repo, kind).await {
        Ok(v) => json_status(StatusCode::CREATED, v),
        Err(e) if e.contains("already") => json_status(StatusCode::CONFLICT, json!({ "error": e })),
        Err(e) if e.contains("Invalid kind") || e.contains("format") || e.contains("not found") => {
            json_status(StatusCode::BAD_REQUEST, json!({ "error": e }))
        }
        Err(e) => json_status(StatusCode::BAD_GATEWAY, json!({ "error": e })),
    }
}

/// PATCH /api/repos/:id — `{ "kind"?, "linked_site_id"? }` (site uuid, `"native"`, or `""`/null to clear).
pub async fn growth_patch_repo(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    let n = js_number(&id);
    let id = if n.is_finite() && n.fract() == 0.0 && n >= 1.0 {
        n as i64
    } else {
        return err(StatusCode::NOT_FOUND, "not_found");
    };
    let parsed = match parse_json(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let kind = as_string(&parsed, "kind");
    let linked_arg: Option<Option<&str>> = match parsed.get("linked_site_id") {
        None => None,
        Some(v) if v.is_null() => Some(None),
        Some(v) => match v.as_str() {
            Some(s) if s.is_empty() => Some(None),
            Some(s) => Some(Some(s)),
            None => return err(StatusCode::BAD_REQUEST, "bad_linked_site_id"),
        },
    };
    match github::patch_repo(&state.db, id, kind, linked_arg).await {
        Ok(Some(v)) => json_status(StatusCode::OK, v),
        Ok(None) => err(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => json_status(StatusCode::BAD_REQUEST, json!({ "error": e })),
    }
}

/// DELETE /api/repos/:id
pub async fn growth_delete_repo(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Response {
    let n = js_number(&id);
    let id = if n.is_finite() && n.fract() == 0.0 && n >= 1.0 {
        n as i64
    } else {
        return err(StatusCode::NOT_FOUND, "not_found");
    };
    match github::delete_repo(&state.db, id).await {
        Ok(Some(repo)) => json_status(
            StatusCode::OK,
            json!({ "message": "Repo removed", "repo": repo }),
        ),
        Ok(None) => err(StatusCode::NOT_FOUND, "not_found"),
        Err(e) => {
            tracing::error!("delete repo failed: {e}");
            err(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
        }
    }
}

/// POST /api/import/github-growth — bulk load from a growth-chart export.
/// Body: `{ "downloads": [...], "metrics": [...] }` (either optional).
/// Max 10k rows total per request (admin; 8 MiB body).
pub async fn growth_import(State(state): State<AppState>, body: Bytes) -> Response {
    const MAX_ROWS: usize = 10_000;
    let parsed = match parse_json(&body) {
        Ok(v) => v,
        Err(resp) => return resp,
    };
    let downloads = parsed
        .get("downloads")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let metrics = parsed
        .get("metrics")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if downloads.is_empty() && metrics.is_empty() {
        return err(StatusCode::BAD_REQUEST, "empty_import");
    }
    if downloads.len() + metrics.len() > MAX_ROWS {
        return err(StatusCode::BAD_REQUEST, "batch_too_large");
    }
    let dl = match github::import_downloads(&state.db, &downloads).await {
        Ok(n) => n,
        Err(e) => {
            tracing::error!("import downloads: {e}");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "server_error");
        }
    };
    let mt = match github::import_metrics(&state.db, &metrics).await {
        Ok(n) => n,
        Err(e) => {
            tracing::error!("import metrics: {e}");
            return err(StatusCode::INTERNAL_SERVER_ERROR, "server_error");
        }
    };
    // Fill any empty site links from GITHUB_SITE_LINKS for the imported repos.
    if let Err(e) = github::seed_repos(&state.db, &state.github).await {
        tracing::warn!(error = %e, "site link backfill after import failed");
    }
    json_status(
        StatusCode::OK,
        json!({
            "ok": true,
            "downloads_imported": dl,
            "metrics_imported": mt,
        }),
    )
}
