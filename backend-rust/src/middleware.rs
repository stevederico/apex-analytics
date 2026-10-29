//! Request middleware — a faithful port of backend/middleware.js: the
//! constant-time key comparison (`safe_equal`), the desktop bearer guard and
//! admin guard, and the fixed-window rate limiter.
//!
//! The app-wide `noindex` header (`X-Robots-Tag`) is applied in `app.rs` via a
//! tower `SetResponseHeaderLayer`, not here — axum/tower has a real middleware
//! chain (unlike the hand-rolled server), so the response header is added once,
//! app-wide, rather than per-handler.
//!
//! AUTH GUARDS: implemented as `axum::middleware::from_fn_with_state` functions
//! that pull headers off the request, consult `AppState`, and either pass the
//! request through (`next.run(req)`) or short-circuit with a 404 EMPTY body.
//! Rejecting with 404 (not 401/403) is deliberate — an unauthenticated probe
//! should see the endpoint as nonexistent. A `Db` error during the bearer
//! lookup is treated as "no user" (deny); guards NEVER allow on error.
//!
//! RATE LIMITING: the `RateLimiter` owns its buckets behind its own `Mutex`, so
//! it is `Sync` and a single `Arc<RateLimiter>` lives in the shared `AppState`.
//! It is attached per-route via `rate_limit_layer(state, max, window_ms)`, which
//! builds a `from_fn` layer closed over the per-route `{max, window}` — matching
//! the JS `rateLimit({ max, windowMs })` factory. We do NOT use tower_governor:
//! its GCRA algorithm has different burst/refill semantics and would break parity
//! with the JS fixed-window behavior the tests pin down.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, Method, Request, StatusCode},
    middleware::Next,
    response::Response,
};

use crate::app::AppState;

/// Constant-time byte comparison — port of node `crypto.timingSafeEqual`
/// semantics wrapped by `safeEqual`. A length mismatch returns false WITHOUT
/// comparing contents (the only timing signal we leak is length, exactly like
/// the JS `aBuf.length !== bBuf.length` early return). For equal lengths, every
/// byte is compared and the differences folded into one accumulator, so there is
/// no early-out on the first mismatch.
pub fn safe_equal(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Mirror key from env: `APEX_MIRROR_KEY`, else the pre-rename `DOTTIE_MIRROR_KEY`.
fn mirror_key_env() -> String {
    crate::app::env_first(&["APEX_MIRROR_KEY", "DOTTIE_MIRROR_KEY"])
}

/// Trimmed `X-Apex-<suffix>` mirror header, else the pre-rename
/// `X-Dottie-<suffix>` (the Wally proxy still sends those).
fn mirror_header(headers: &HeaderMap, suffix: &str) -> Option<String> {
    header_trim(headers, &format!("x-apex-{suffix}"))
        .or_else(|| header_trim(headers, &format!("x-dottie-{suffix}")))
}

/// True when `X-Apex-Mirror-Key` (or `X-Dottie-Mirror-Key`) matches the
/// mirror key. The Wally proxy sets this so a server-side forward can pass
/// the real visitor IP and country. A missing or wrong key is ignored.
fn mirror_trusted(headers: &HeaderMap, mirror_key: &str) -> bool {
    if mirror_key.is_empty() {
        return false;
    }
    let Some(got) = mirror_header(headers, "mirror-key") else {
        return false;
    };
    safe_equal(mirror_key.as_bytes(), got.as_bytes())
}

fn header_trim(headers: &HeaderMap, name: &str) -> Option<String> {
    let t = headers.get(name)?.to_str().ok()?.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
}

/// Client IP: a trusted mirror's `X-Apex-Visitor-Ip` (or `X-Dottie-`), else `CF-Connecting-IP`
/// (Cloudflare sets this; not spoofable at the origin), else first
/// `X-Forwarded-For` segment, else `X-Real-IP`, else `"unknown"`.
pub fn client_ip(headers: &HeaderMap) -> String {
    let key = mirror_key_env();
    client_ip_for(headers, &key)
}

pub(crate) fn client_ip_for(headers: &HeaderMap, mirror_key: &str) -> String {
    if mirror_trusted(headers, mirror_key) {
        if let Some(ip) = mirror_header(headers, "visitor-ip") {
            return ip;
        }
    }
    if let Some(cf) = headers
        .get("cf-connecting-ip")
        .and_then(|v| v.to_str().ok())
    {
        let t = cf.trim();
        if !t.is_empty() {
            return t.to_string();
        }
    }
    if let Some(xff) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
        if let Some(first) = xff.split(',').next() {
            let t = first.trim();
            if !t.is_empty() {
                return t.to_string();
            }
        }
    }
    if let Some(real) = headers.get("x-real-ip").and_then(|v| v.to_str().ok()) {
        if !real.is_empty() {
            return real.to_string();
        }
    }
    "unknown".to_string()
}

/// User-Agent for a live hit. Cloudflare only lets the Dottie user-agent
/// through, so a trusted mirror sends that on `User-Agent` and the visitor's
/// real agent on `X-Apex-User-Agent` (or `X-Dottie-User-Agent`).
pub fn visitor_user_agent(headers: &HeaderMap) -> String {
    let key = mirror_key_env();
    visitor_user_agent_for(headers, &key)
}

pub(crate) fn visitor_user_agent_for(headers: &HeaderMap, mirror_key: &str) -> String {
    if mirror_trusted(headers, mirror_key) {
        if let Some(ua) = mirror_header(headers, "user-agent") {
            return ua;
        }
    }
    header_trim(headers, "user-agent").unwrap_or_default()
}

/// True when this request carries a matching mirror key. Cloudflare bot
/// headers on that request describe the proxy, not the visitor.
pub(crate) fn mirror_trusted_env(headers: &HeaderMap) -> bool {
    let key = mirror_key_env();
    mirror_trusted(headers, &key)
}

/// Country for a live hit. A trusted mirror may pass `X-Apex-Country` (or `X-Dottie-`)
/// (the visitor's `CF-IPCountry` from the edge in front of `aob`). Otherwise
/// use `CF-IPCountry` on this request. `XX` and anything that is not 2 letters
/// is treated as unknown.
pub fn visitor_country(headers: &HeaderMap) -> Option<String> {
    let key = mirror_key_env();
    visitor_country_for(headers, &key)
}

pub(crate) fn visitor_country_for(headers: &HeaderMap, mirror_key: &str) -> Option<String> {
    let raw = if mirror_trusted(headers, mirror_key) {
        mirror_header(headers, "country")
            .or_else(|| header_trim(headers, "cf-ipcountry"))
    } else {
        header_trim(headers, "cf-ipcountry")
    }?;
    if raw.len() == 2 && raw != "XX" {
        Some(raw.to_uppercase())
    } else {
        None
    }
}

/// Read a header as a `&str`, or `""` when absent/non-UTF-8 (matches the JS
/// `c.req.header(name) ?? ""`).
fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers.get(name).and_then(|v| v.to_str().ok()).unwrap_or("")
}

/// A 404 with an EMPTY body — the reject response for both guards (and the JS
/// `c.body(null, 404)`).
fn body_404() -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::empty())
        .expect("static 404 response builds")
}

/// The user a desktop request's bearer resolved to.
#[derive(Debug, Clone)]
pub struct AuthedUser(pub crate::db::UserRow);

/// Desktop ingest guard, as axum middleware: `Authorization: Bearer <tok>`
/// must resolve to a user via `Db`, else reject with a 404 empty body. A `Db`
/// error during the lookup = deny (never allow on error). The old shared
/// `X-Dottie-Client` key is retired: desktop has sent only the per-user bearer
/// since 2026.5.2.1, and no older build has pinged since 2026-05-11.
pub async fn desktop_bearer_guard(
    State(state): State<AppState>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let auth = header_str(req.headers(), "authorization").to_string();
    if let Some(token) = auth.strip_prefix("Bearer ") {
        // Ok(None) (unknown token) and Err(_) (DB error) both reject.
        if let Ok(Some(user)) = state.db.get_user_by_token(token).await {
            // Ingest handlers use this to link a Pro-registered signup to the
            // install that is pinging with its bearer.
            req.extensions_mut().insert(AuthedUser(user));
            return next.run(req).await;
        }
    }
    body_404()
}

/// Admin guard (port of `adminKeyGuard`), as axum middleware: `Authorization:
/// Bearer <tok>` must constant-time-equal a NON-EMPTY `admin_key`, else reject
/// with a 404 empty body.
pub async fn admin_key_guard(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let auth = header_str(req.headers(), "authorization");
    let provided = auth.strip_prefix("Bearer ").unwrap_or("");
    if state.admin_key.is_empty()
        || !safe_equal(provided.as_bytes(), state.admin_key.as_bytes())
    {
        return body_404();
    }
    next.run(req).await
}

/// Report guard: like `admin_key_guard` but method-aware. Safe (GET/HEAD)
/// requests accept EITHER a non-empty `admin_key` OR a non-empty `read_key`;
/// any mutating method (POST/PATCH/DELETE/PUT) accepts ONLY the `admin_key`.
/// This lets the read-only report key call every stats/report/site GET without
/// ever unlocking a write on the same router (e.g. `POST /api/sites`,
/// `POST /api/downloads/snapshot`). Wrong/missing key → 404, as everywhere.
pub async fn report_key_guard(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let auth = header_str(req.headers(), "authorization");
    let provided = auth.strip_prefix("Bearer ").unwrap_or("").as_bytes();
    let admin_ok =
        !state.admin_key.is_empty() && safe_equal(provided, state.admin_key.as_bytes());
    let is_read = matches!(*req.method(), Method::GET | Method::HEAD);
    let read_ok = is_read
        && !state.read_key.is_empty()
        && safe_equal(provided, state.read_key.as_bytes());
    if !admin_ok && !read_ok {
        return body_404();
    }
    next.run(req).await
}

/// Pull one query-string parameter's raw value, e.g. `key` from
/// `?format=md&key=abc`. Values are hex tokens (no percent-encoding), so a plain
/// `split` is enough. Returns "" when absent.
// yagni: naive split; switch to form_urlencoded if a param can carry reserved chars.
fn query_param<'a>(query: Option<&'a str>, name: &str) -> &'a str {
    let Some(q) = query else { return "" };
    for pair in q.split('&') {
        if let Some(v) = pair.strip_prefix(name).and_then(|r| r.strip_prefix('=')) {
            return v;
        }
    }
    ""
}

/// Report-composite guard for the browser-friendly MD endpoints
/// (`/api/report/daily`, `/api/report/site-daily`). Accepts, on GET/HEAD only:
/// the `admin_key` or `read_key` via `Authorization: Bearer`, OR the `read_key`
/// via a `?key=` query param (so a status page / webhook can fetch the report
/// with a plain URL — no header). The query path NEVER accepts the admin key
/// (query strings leak into logs/history) and never unlocks a write. Wrong or
/// missing key → 404, as everywhere.
pub async fn report_public_guard(
    State(state): State<AppState>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let is_read = matches!(*req.method(), Method::GET | Method::HEAD);
    let auth = header_str(req.headers(), "authorization");
    let bearer = auth.strip_prefix("Bearer ").unwrap_or("").as_bytes();
    let admin_ok = !state.admin_key.is_empty() && safe_equal(bearer, state.admin_key.as_bytes());
    let read_key_set = is_read && !state.read_key.is_empty();
    let read_header_ok = read_key_set && safe_equal(bearer, state.read_key.as_bytes());
    let read_query_ok = read_key_set
        && safe_equal(
            query_param(req.uri().query(), "key").as_bytes(),
            state.read_key.as_bytes(),
        );
    if !admin_ok && !read_header_ok && !read_query_ok {
        return body_404();
    }
    next.run(req).await
}

/// A single fixed-window bucket: request count + the absolute reset time (ms).
#[derive(Clone, Copy)]
struct Bucket {
    count: u32,
    reset_at: u64,
}

/// Hard cap on tracked keys, matching middleware.js `MAX_BUCKETS`. When full, an
/// arbitrary entry is evicted to make room (the JS evicts the Map's
/// first-inserted key; we evict an arbitrary entry — see `check_at`).
const MAX_BUCKETS: usize = 5000;

/// Fixed-window rate limiter, self-contained and `Sync`: all state lives behind
/// this struct's own `Mutex`, so one instance can sit in shared `AppState` and
/// be hit from every worker thread. Port of the `rateLimit` factory + the module
/// `buckets` Map + the 60s sweep interval in middleware.js.
pub struct RateLimiter {
    buckets: Mutex<HashMap<String, Bucket>>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new()
    }
}

impl RateLimiter {
    pub fn new() -> RateLimiter {
        RateLimiter {
            buckets: Mutex::new(HashMap::new()),
        }
    }

    /// `true` = allowed, `false` = over the limit (the route returns 429).
    /// Uses the real clock; tests use `check_at` to inject `now`.
    pub fn check(&self, path: &str, ip: &str, max: u32, window_ms: u64) -> bool {
        self.check_at(now_ms(), path, ip, max, window_ms)
    }

    /// `check` with an injected `now_ms` (testable without sleeping). Mirrors
    /// middleware.js exactly: key = "{path}:{ip}"; a missing OR expired window
    /// resets to count=1 (and, when at the bucket cap, first evicts one entry);
    /// otherwise increment, rejecting only when the new count EXCEEDS `max`.
    pub fn check_at(&self, now_ms: u64, path: &str, ip: &str, max: u32, window_ms: u64) -> bool {
        let key = format!("{path}:{ip}");
        let mut buckets = match self.buckets.lock() {
            Ok(g) => g,
            // A poisoned lock shouldn't take the server down; fail OPEN (allow)
            // rather than deny-all, matching the JS where this can't panic.
            Err(p) => p.into_inner(),
        };
        match buckets.get(&key).copied() {
            Some(entry) if now_ms <= entry.reset_at => {
                // Live window: increment and reject past max.
                let next = entry.count + 1;
                buckets.insert(
                    key,
                    Bucket {
                        count: next,
                        reset_at: entry.reset_at,
                    },
                );
                next <= max
            }
            _ => {
                // New or expired window — reset to count=1. Evict first if full.
                if buckets.len() >= MAX_BUCKETS && !buckets.contains_key(&key) {
                    // HashMap has no stable insertion order, so we evict an
                    // arbitrary key (the JS evicts the oldest; either keeps the
                    // map bounded, which is the only invariant that matters).
                    if let Some(victim) = buckets.keys().next().cloned() {
                        buckets.remove(&victim);
                    }
                }
                buckets.insert(
                    key,
                    Bucket {
                        count: 1,
                        reset_at: now_ms + window_ms,
                    },
                );
                true
            }
        }
    }

    /// Drop expired buckets — port of the 60s `setInterval` sweep. A background
    /// timer wires to this; exposed here so it's testable.
    pub fn sweep(&self, now_ms: u64) {
        let mut buckets = match self.buckets.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        buckets.retain(|_, b| now_ms <= b.reset_at);
    }

    /// Current bucket count — for tests/diagnostics only.
    fn len(&self) -> usize {
        match self.buckets.lock() {
            Ok(g) => g.len(),
            Err(p) => p.into_inner().len(),
        }
    }
}

/// Per-route rate-limit config — the axum analogue of the JS
/// `rateLimit({ max, windowMs })` factory's captured args. Carried as the
/// `from_fn_with_state` state alongside the `AppState` so a single generic
/// middleware function serves every route with its own `{max, window}`.
#[derive(Clone, Copy)]
pub struct RateLimit {
    pub max: u32,
    pub window_ms: u64,
}

/// Rate-limit middleware. Keyed on the matched path + client IP, exactly like
/// the JS `${c.req.path}:${ip}`. On exceed it short-circuits with a 429 empty
/// body (matching `c.body(null, 429)`); otherwise it passes through. The `cfg`
/// (per-route `{max, window}`) is injected as state via `from_fn_with_state`.
pub async fn rate_limit_mw(
    State((state, cfg)): State<(AppState, RateLimit)>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();
    let ip = client_ip(req.headers());
    if state
        .rate_limiter
        .check(&path, &ip, cfg.max, cfg.window_ms)
    {
        next.run(req).await
    } else {
        Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .body(Body::empty())
            .expect("static 429 response builds")
    }
}

/// Unix milliseconds, matching JS `Date.now()`.
fn now_ms() -> u64 {
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(dur.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    // ---- safe_equal -----------------------------------------------------

    #[test]
    fn safe_equal_equal_strings() {
        assert!(safe_equal(b"secret-key", b"secret-key"));
        assert!(safe_equal(b"", b""), "empty == empty");
    }

    #[test]
    fn safe_equal_unequal_same_length() {
        assert!(!safe_equal(b"secret-key", b"secret-keX"));
        assert!(!safe_equal(b"aaaa", b"bbbb"));
        // Difference in the LAST byte must still be caught (no early-out, full
        // scan): confirms the fold covers the whole length.
        assert!(!safe_equal(b"abcd", b"abce"));
    }

    #[test]
    fn safe_equal_different_length() {
        assert!(!safe_equal(b"short", b"longer-value"));
        assert!(!safe_equal(b"abc", b"ab"));
        assert!(!safe_equal(b"", b"x"));
    }

    // ---- rate limiter ---------------------------------------------------

    #[test]
    fn allows_up_to_max_rejects_over() {
        let rl = RateLimiter::new();
        // max=10: first 10 allowed, 11th rejected (matches /api/register).
        for i in 1..=10 {
            assert!(
                rl.check_at(1_000, "/api/register", "1.1.1.1", 10, 60_000),
                "request {i} within limit should pass"
            );
        }
        assert!(
            !rl.check_at(1_000, "/api/register", "1.1.1.1", 10, 60_000),
            "11th request must be rejected"
        );
    }

    #[test]
    fn separate_keys_are_independent() {
        let rl = RateLimiter::new();
        // Different IP -> different bucket -> own allowance.
        assert!(rl.check_at(0, "/p", "a", 1, 1000));
        assert!(!rl.check_at(0, "/p", "a", 1, 1000), "a over limit");
        assert!(rl.check_at(0, "/p", "b", 1, 1000), "b is independent");
        // Different path -> different bucket too.
        assert!(rl.check_at(0, "/q", "a", 1, 1000), "path q independent of p");
    }

    #[test]
    fn resets_after_window_via_injected_now() {
        let rl = RateLimiter::new();
        let win = 60_000;
        assert!(rl.check_at(0, "/p", "ip", 1, win));
        assert!(!rl.check_at(0, "/p", "ip", 1, win), "second within window rejected");
        // At now == reset_at (0 + win) the window is still live (`now <= reset_at`).
        assert!(
            !rl.check_at(win, "/p", "ip", 1, win),
            "exactly at reset boundary still within window"
        );
        // Past the boundary -> fresh window, count resets to 1, allowed again.
        assert!(
            rl.check_at(win + 1, "/p", "ip", 1, win),
            "after window expires the bucket resets"
        );
    }

    #[test]
    fn evicts_at_cap() {
        let rl = RateLimiter::new();
        // Fill to exactly MAX_BUCKETS distinct keys.
        for i in 0..MAX_BUCKETS {
            assert!(rl.check_at(0, "/p", &format!("ip-{i}"), 5, 60_000));
        }
        assert_eq!(rl.len(), MAX_BUCKETS, "map filled to cap");
        // One more NEW key must trigger an eviction, keeping size bounded.
        assert!(rl.check_at(0, "/p", "overflow-ip", 5, 60_000));
        assert_eq!(rl.len(), MAX_BUCKETS, "stays bounded at cap after eviction");
    }

    #[test]
    fn sweep_removes_expired_buckets() {
        let rl = RateLimiter::new();
        rl.check_at(0, "/p", "a", 5, 1_000); // reset_at = 1000
        rl.check_at(0, "/p", "b", 5, 10_000); // reset_at = 10000
        assert_eq!(rl.len(), 2);
        // now=5000: bucket a expired (1000), b live (10000).
        rl.sweep(5_000);
        assert_eq!(rl.len(), 1, "only the expired bucket is swept");
        // The live one still rate-limits.
        assert!(rl.check_at(5_000, "/p", "b", 5, 10_000));
    }

    // ---- client_ip ------------------------------------------------------

    fn headers_from(pairs: &[(&str, &str)]) -> HeaderMap {
        use axum::http::HeaderName;
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        h
    }

    #[test]
    fn client_ip_extraction() {
        assert_eq!(
            client_ip(&headers_from(&[("x-forwarded-for", "203.0.113.1, 10.0.0.1")])),
            "203.0.113.1",
            "first XFF segment, trimmed"
        );
        assert_eq!(
            client_ip(&headers_from(&[("x-real-ip", "198.51.100.7")])),
            "198.51.100.7",
            "falls back to X-Real-IP"
        );
        assert_eq!(
            client_ip(&headers_from(&[])),
            "unknown",
            "no headers -> unknown"
        );
        assert_eq!(
            client_ip(&headers_from(&[
                ("cf-connecting-ip", "198.51.100.9"),
                ("x-forwarded-for", "203.0.113.1, 10.0.0.1"),
            ])),
            "198.51.100.9",
            "CF-Connecting-IP wins over spoofable XFF"
        );
        assert_eq!(
            client_ip_for(
                &headers_from(&[
                    ("x-dottie-mirror-key", "sekret"),
                    ("x-dottie-visitor-ip", "203.0.113.8"),
                    ("cf-connecting-ip", "198.51.100.9"),
                ]),
                "sekret",
            ),
            "203.0.113.8",
            "trusted mirror IP wins"
        );
        assert_eq!(
            client_ip_for(
                &headers_from(&[
                    ("x-dottie-mirror-key", "nope"),
                    ("x-dottie-visitor-ip", "203.0.113.8"),
                    ("cf-connecting-ip", "198.51.100.9"),
                ]),
                "sekret",
            ),
            "198.51.100.9",
            "wrong mirror key is ignored"
        );
        assert_eq!(
            visitor_country_for(
                &headers_from(&[
                    ("x-dottie-mirror-key", "sekret"),
                    ("x-dottie-country", "de"),
                    ("cf-ipcountry", "US"),
                ]),
                "sekret",
            ),
            Some("DE".to_string())
        );
        assert_eq!(
            visitor_user_agent_for(
                &headers_from(&[
                    ("x-dottie-mirror-key", "sekret"),
                    ("user-agent", "Mozilla/5.0 Dottie/2026.7.6"),
                    ("x-dottie-user-agent", "Mozilla/5.0 Chrome/128.0.0.0"),
                ]),
                "sekret",
            ),
            "Mozilla/5.0 Chrome/128.0.0.0",
            "trusted mirror keeps the visitor user-agent"
        );
        assert_eq!(
            visitor_user_agent_for(
                &headers_from(&[
                    ("user-agent", "Mozilla/5.0 Dottie/2026.7.6"),
                    ("x-dottie-user-agent", "Mozilla/5.0 Chrome/128.0.0.0"),
                ]),
                "sekret",
            ),
            "Mozilla/5.0 Dottie/2026.7.6",
            "without the mirror key the visitor user-agent is ignored"
        );
        assert_eq!(
            client_ip_for(
                &headers_from(&[
                    ("x-apex-mirror-key", "sekret"),
                    ("x-apex-visitor-ip", "203.0.113.9"),
                    ("x-dottie-visitor-ip", "203.0.113.8"),
                ]),
                "sekret",
            ),
            "203.0.113.9",
            "X-Apex-* mirror headers win over the pre-rename X-Dottie-*"
        );
    }
}
