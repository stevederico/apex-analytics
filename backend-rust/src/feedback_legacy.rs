//! Compatibility shim for the retired feedback-assistant service, so pages
//! that still embed its widget, or call its `/v1` API directly, keep landing
//! feedback here. A reverse proxy forwards these paths from the old service's
//! hostnames; they also answer on this service's own host.
//!
//! Keys are the old app keys (`pk_…`), mapped to the site each app moved to by
//! `FEEDBACK_LEGACY_KEYS` (see `feedback::resolve_site`). Unknown key → empty
//! 404, like every other keyed route here (the old service said 401).
//!
//! PUBLIC (same 20/min per IP and body cap as `/api/feedback`):
//!   - `GET /widget.js`, `GET /widget/v<any>.js` — the current widget. It reads
//!     the old tag's `data-project` when `data-website-id` is absent and posts
//!     to `/api/feedback` on the host that served it.
//!   - `GET /widget/html2canvas-v1.js` — the current screenshot library.
//!   - `POST /v1/screenshots` — `X-Project-Key` + multipart `file` →
//!     `{screenshotId}`. Held in memory for `STAGED_TTL_MS` until the
//!     submission that references it arrives (the old two-step upload).
//!   - `POST /v1/submissions` — `X-Project-Key` + JSON `{message, url,
//!     userAgent, appVersion, endUserId, endUserName, endUserEmail,
//!     screenshotId?}` → `{ok, submissionId}`. Writes one `feedback` row.
//!   - `GET /v1/projects/{key}/widget` → `{greeting: null}`,
//!     `GET /v1/projects/{key}/changelog` → `{changelog: []}` (both features
//!     are gone; the old widget renders nothing for them).
//!   - `OPTIONS` on the `/v1` routes: `X-Project-Key` is a custom header, so
//!     browsers preflight.

use std::collections::HashMap;
use std::hash::{BuildHasher, Hasher};
use std::sync::{Mutex, OnceLock};

use axum::{
    body::{Body, Bytes},
    extract::{Path, State},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::app::AppState;
use crate::feedback::{
    empty_status, err, now_ms, parse_multipart, resolve_site, store_submission, widget_file,
    with_cors, Submission, MAX_SCREENSHOT_BYTES,
};

/// How long an uploaded screenshot waits for its submission.
const STAGED_TTL_MS: i64 = 15 * 60 * 1000;

/// Most screenshots held at once; the oldest is dropped past this.
const MAX_STAGED: usize = 64;

/// A screenshot uploaded through `/v1/screenshots`, not yet attached.
struct Staged {
    site_id: String,
    content_type: String,
    bytes: Vec<u8>,
    at_ms: i64,
}

fn staged() -> &'static Mutex<HashMap<String, Staged>> {
    static STAGED: OnceLock<Mutex<HashMap<String, Staged>>> = OnceLock::new();
    STAGED.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Unguessable id: SHA-256 of a per-process random seed, the clock and a counter.
fn new_screenshot_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let mut seed = std::collections::hash_map::RandomState::new().build_hasher();
    seed.write_u64(SEQ.fetch_add(1, Ordering::Relaxed));
    let mut h = Sha256::new();
    h.update(seed.finish().to_le_bytes());
    h.update(now_ms().to_le_bytes());
    h.update(SEQ.load(Ordering::Relaxed).to_le_bytes());
    hex::encode(&h.finalize()[..16])
}

/// Drop expired entries, then the oldest while over the cap.
fn prune(map: &mut HashMap<String, Staged>, now: i64) {
    map.retain(|_, s| now - s.at_ms < STAGED_TTL_MS);
    while map.len() >= MAX_STAGED {
        let Some(oldest) = map.iter().min_by_key(|(_, s)| s.at_ms).map(|(k, _)| k.clone()) else {
            break;
        };
        map.remove(&oldest);
    }
}

fn project_key(headers: &HeaderMap) -> Option<&str> {
    headers.get("x-project-key").and_then(|v| v.to_str().ok())
}

// ---- widget URLs ---------------------------------------------------------------

/// `GET /widget.js` — the old auto-updating widget URL.
pub async fn widget_js() -> Response {
    widget_file("feedback.js", "public, max-age=300, must-revalidate")
}

/// `GET /widget/{file}` — `v<version>.js` pins (the old service only served its
/// current version, so pinned tags 404ed) and the old html2canvas path.
pub async fn widget_path(Path(file): Path<String>) -> Response {
    if file == "html2canvas-v1.js" {
        return crate::feedback::widget_html2canvas_v1().await;
    }
    match file.strip_prefix('v').and_then(|v| v.strip_suffix(".js")) {
        Some(ver) if !ver.is_empty() && !ver.contains('/') => {
            widget_file("feedback.js", "public, max-age=300, must-revalidate")
        }
        _ => empty_status(StatusCode::NOT_FOUND),
    }
}

// ---- /v1 API -------------------------------------------------------------------

/// Preflight for the `/v1` POSTs (custom `X-Project-Key` header).
pub async fn v1_options(headers: HeaderMap) -> Response {
    let resp = Response::builder()
        .status(StatusCode::NO_CONTENT)
        .header(header::ACCESS_CONTROL_ALLOW_METHODS, "GET, POST")
        .header(header::ACCESS_CONTROL_ALLOW_HEADERS, "Content-Type, X-Project-Key")
        .header(header::ACCESS_CONTROL_MAX_AGE, "86400")
        .body(Body::empty())
        .expect("legacy OPTIONS builds");
    with_cors(&headers, resp)
}

/// `POST /v1/screenshots` — stage one image for a later `/v1/submissions`.
pub async fn v1_screenshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let site_id = match resolve_site(&state, project_key(&headers)).await {
        Ok(Some(id)) => id,
        Ok(None) => return empty_status(StatusCode::NOT_FOUND),
        Err(resp) => return with_cors(&headers, resp),
    };
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    let Some(parts) = parse_multipart(content_type, &body) else {
        return with_cors(&headers, err(StatusCode::BAD_REQUEST, "invalid_multipart"));
    };
    let Some(file) = parts.into_iter().find(|p| p.name == "file" && !p.data.is_empty()) else {
        return with_cors(&headers, err(StatusCode::BAD_REQUEST, "file_required"));
    };
    if file.content_type != "image/jpeg" && file.content_type != "image/png" {
        return with_cors(&headers, err(StatusCode::UNSUPPORTED_MEDIA_TYPE, "screenshot_type"));
    }
    if file.data.len() > MAX_SCREENSHOT_BYTES {
        return with_cors(&headers, err(StatusCode::PAYLOAD_TOO_LARGE, "screenshot_too_large"));
    }
    let id = new_screenshot_id();
    let now = now_ms();
    if let Ok(mut map) = staged().lock() {
        prune(&mut map, now);
        map.insert(
            id.clone(),
            Staged { site_id, content_type: file.content_type, bytes: file.data.to_vec(), at_ms: now },
        );
    }
    with_cors(&headers, Json(json!({ "screenshotId": id })).into_response())
}

/// `POST /v1/submissions` — one old-widget submission → one `feedback` row.
pub async fn v1_submission(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let site_id = match resolve_site(&state, project_key(&headers)).await {
        Ok(Some(id)) => id,
        Ok(None) => return empty_status(StatusCode::NOT_FOUND),
        Err(resp) => return with_cors(&headers, resp),
    };
    let v: Value = match serde_json::from_slice(&body) {
        Ok(v @ Value::Object(_)) => v,
        _ => return with_cors(&headers, err(StatusCode::BAD_REQUEST, "invalid_json")),
    };
    let s = |k: &str| v.get(k).and_then(Value::as_str).filter(|x| !x.is_empty()).map(str::to_string);
    let screenshot = match s("screenshotId") {
        None => None,
        Some(id) => {
            let taken = staged().lock().ok().and_then(|mut map| {
                let hit = map.get(&id).is_some_and(|st| st.site_id == site_id && now_ms() - st.at_ms < STAGED_TTL_MS);
                if hit { map.remove(&id) } else { None }
            });
            match taken {
                Some(st) => Some((st.content_type, st.bytes)),
                None => return with_cors(&headers, err(StatusCode::BAD_REQUEST, "unknown_screenshot")),
            }
        }
    };
    let sub = Submission {
        website: None,
        message: s("message").unwrap_or_default(),
        url: s("url"),
        user_agent: s("userAgent"),
        app_version: s("appVersion"),
        end_user_id: s("endUserId"),
        end_user_name: s("endUserName"),
        end_user_email: s("endUserEmail"),
        screenshot,
    };
    match store_submission(&state, &headers, &site_id, &sub).await {
        Ok(id) => with_cors(
            &headers,
            Json(json!({ "ok": true, "submissionId": id.to_string() })).into_response(),
        ),
        Err(resp) => with_cors(&headers, resp),
    }
}

/// `GET /v1/projects/{key}/{widget|changelog}` — greeting and What's New are
/// gone; answer the empty shapes the old widget already handles.
pub async fn v1_project(
    headers: HeaderMap,
    Path((_key, what)): Path<(String, String)>,
) -> Response {
    let body = match what.as_str() {
        "widget" => json!({ "greeting": null }),
        "changelog" => json!({ "changelog": [] }),
        _ => return empty_status(StatusCode::NOT_FOUND),
    };
    let mut resp = Json(body).into_response();
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("public, max-age=60"),
    );
    with_cors(&headers, resp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(at_ms: i64) -> Staged {
        Staged { site_id: "s".into(), content_type: "image/png".into(), bytes: vec![1], at_ms }
    }

    #[test]
    fn prune_drops_expired_then_oldest() {
        let mut m = HashMap::new();
        m.insert("old".to_string(), st(0));
        m.insert("fresh".to_string(), st(STAGED_TTL_MS));
        prune(&mut m, STAGED_TTL_MS + 1);
        assert!(!m.contains_key("old"));
        assert!(m.contains_key("fresh"));

        let mut m: HashMap<String, Staged> = (0..MAX_STAGED as i64).map(|i| (i.to_string(), st(1000 + i))).collect();
        prune(&mut m, 1000 + MAX_STAGED as i64);
        assert_eq!(m.len(), MAX_STAGED - 1);
        assert!(!m.contains_key("0"));
    }

    #[test]
    fn screenshot_ids_are_unique_hex() {
        let a = new_screenshot_id();
        let b = new_screenshot_id();
        assert_ne!(a, b);
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    }
}
