//! Application wiring — the shared `AppState`, the version config, and the
//! `build_app` router factory that applies the app-wide tower layers.
//!
//! STATE: `AppState` is `Clone` (every field is cheap to clone — `Db` wraps an
//! `Arc<Pool>`, the `RateLimiter` is behind an `Arc`, the keys/version are
//! plain `String`s in an `Arc`-free struct cloned by value). It's the axum
//! `State` every handler and guard extracts. Auth keys + version config are read
//! from env in `from_env`; tests construct it directly via `new`.
//!
//! LAYERS (app-wide, applied in `build_app`):
//!   - `SetResponseHeaderLayer` adds `X-Robots-Tag: noindex, nofollow, noarchive`
//!     to EVERY response (port of the JS `noindex` middleware).
//!   - `DefaultBodyLimit::max(4096)` caps request bodies at 4 KiB → 413 on
//!     exceed (port of the JS `MAX_BODY_BYTES` content-length guard). It's
//!     enforced when a handler's extractor reads the body; the rejection still
//!     flows back through the outer `SetResponseHeaderLayer`, so the 413 carries
//!     `X-Robots-Tag` like every other response.
//!   - `TimeoutLayer(15s)` is prod hardening — a slow/stuck handler returns 408
//!     rather than holding a connection forever.
//!   - `TraceLayer` for per-request tracing spans.
//!
//! CORS is intentionally NOT global. The landing routes allow one cross-origin
//! caller, `LANDING_ALLOWED_ORIGIN` (unset → refused), and the public ingest
//! routes (`/api/send`, `/api/collect`, `/api/feedback`) reflect site origins.
//! All of it is set in-handler; there is no tower `CorsLayer`. Everything else
//! is same-origin / server-to-server and needs no CORS.

use std::time::Duration;

use axum::{
    body::Body,
    extract::DefaultBodyLimit,
    http::{header, HeaderValue, StatusCode},
    response::Response,
    routing::{delete, get, patch, post, MethodFilter},
    Router,
};
use tower::ServiceBuilder;
use tower_http::{
    decompression::RequestDecompressionLayer,
    set_header::SetResponseHeaderLayer,
    timeout::TimeoutLayer,
    trace::TraceLayer,
};

use crate::backup;
use crate::db::Db;
use crate::feedback;
use crate::feedback_legacy;
use crate::github::GithubConfig;
use crate::grpc;
use crate::middleware::{
    admin_key_guard, desktop_bearer_guard, rate_limit_mw, report_key_guard,
    report_public_guard, RateLimit, RateLimiter,
};
use crate::routes;
use tonic::service::Routes as TonicRoutes;

/// Max request body in bytes — port of the JS `MAX_BODY_BYTES`. Over this → 413.
pub const MAX_BODY_BYTES: usize = 4096;

/// Body cap for single-event ingest (`POST /api/events`). Crash and hang
/// events carry a redacted log tail (≤2 KiB) plus a stack (≤3000 chars), so
/// they exceed the 4 KiB app-wide cap and were rejected with 413 at the
/// extractor — before the handler, so nothing logged it. Those are the events
/// with the most diagnostic value. 32 KiB × the route's 120/min rate limit is
/// still a bounded worst case per IP; every other route keeps 4 KiB.
pub const MAX_EVENT_BODY_BYTES: usize = 32 * 1024;

/// Larger body cap for the batch ingest route only — a batch of up to 100 events
/// won't fit in the 4 KiB single-event limit. Applied route-scoped so every other
/// route keeps the tight 4 KiB cap. Also bounds a gzip-inflated body (the
/// decompression layer is outer, this limit is measured on the inflated bytes).
pub const MAX_BATCH_BODY_BYTES: usize = 256 * 1024;

/// Client-version policy surfaced to the desktop app (latest available, minimum
/// supported, where to download). Ported from server.js's version config; values
/// come from env with the documented defaults.
#[derive(Debug, Clone)]
pub struct VersionConfig {
    pub latest: String,
    pub min: String,
    pub download_url: String,
    /// Provider a fresh install should select once it has a registration token
    /// (`DOTTIE_DEFAULT_PROVIDER`, default "dottiepro" so new users land on the
    /// free Pro trial). Served from /api/version; the client applies it only
    /// when the user has not chosen a provider themselves.
    pub default_provider: String,
    /// Bring-your-own-key model pickers, keyed by ChatProvider raw value
    /// ("openai", "anthropic", "cerebras"). These run on the *user's* key, so
    /// unlike the Pro list they are presentation only — nothing here is
    /// enforced or billable to us. Env: `BYOK_MODELS_<PROVIDER>`.
    pub provider_models: Vec<(String, Vec<crate::models::ProModel>)>,
    /// Model for "what's on my screen" via the xAI vision path (`VISION_MODEL`).
    /// Must be a vision-capable Grok id.
    pub vision_model: String,
    /// Default on-device GGUFs (`LOCAL_MODEL`, `LOCAL_MODEL_HIGH_MEM`). The
    /// client still picks between the two tiers by installed RAM; this only
    /// changes which repos those tiers point at.
    pub local_model: String,
    pub local_model_high_mem: String,
    /// Base URL the gateway pulls parakeet STT GGUFs from
    /// (`STT_MODEL_URL_BASE`), so a repo move upstream is config, not a release.
    pub stt_model_url_base: String,
}

/// Shared application state: the data layer, the rate limiter, the auth keys,
/// version config, and GitHub growth config. Cheap to clone (see module docs).
#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub rate_limiter: std::sync::Arc<RateLimiter>,
    pub admin_key: String,
    /// Optional read-only report key. Non-empty → GET report/stats/site
    /// endpoints accept it as an alternative bearer; it NEVER unlocks writes
    /// (POST/PATCH/DELETE) or the admin SPA/gRPC path. Empty → feature off.
    pub read_key: String,
    pub version: VersionConfig,
    pub github: GithubConfig,
    /// Pro model picker for `/api/version`. Enforcement lives on dottie-pro.
    pub pro_models: Vec<crate::models::ProModel>,
    /// Daily report pulls fleet + pending refunds from dottie-pro. Empty → skip
    /// the fetch (`PRO_STATS_URL` unset).
    pub pro_stats_url: String,
    /// Extra host the Pro stats fetch may reach besides loopback
    /// (`PRO_STATS_ALLOWED_HOST`). Empty → loopback only.
    pub pro_stats_allowed_host: String,
    /// The only origin allowed to call the landing routes cross-origin
    /// (`LANDING_ALLOWED_ORIGIN`). Empty → every landing call is refused.
    pub landing_origin: String,
}

/// Env var or a compiled-in default, treating an empty value as unset so a
/// blank Railway variable can never erase a working default.
fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key).ok().filter(|v| !v.is_empty()).unwrap_or_else(|| fallback.to_string())
}

/// First non-empty value among `keys`, else "". Lets a renamed variable
/// (`APEX_*`) win while the pre-rename name (`DOTTIE_*`) keeps working.
pub fn env_first(keys: &[&str]) -> String {
    keys.iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_default()
}

impl AppState {
    /// Construct from explicit pieces (used by tests and by `from_env`).
    pub fn new(db: Db, admin_key: String, version: VersionConfig) -> AppState {
        AppState {
            db,
            rate_limiter: std::sync::Arc::new(RateLimiter::new()),
            admin_key,
            read_key: String::new(),
            version,
            github: GithubConfig::default(),
            pro_models: Vec::new(),
            pro_stats_url: String::new(),
            pro_stats_allowed_host: String::new(),
            landing_origin: String::new(),
        }
    }

    /// Like `new` but with an allowed landing origin.
    pub fn with_landing_origin(mut self, landing_origin: String) -> AppState {
        self.landing_origin = landing_origin;
        self
    }

    /// Like `new` but with an explicit GitHub growth config.
    pub fn with_github(mut self, github: GithubConfig) -> AppState {
        self.github = github;
        self
    }

    /// Like `new` but with an explicit read-only report key.
    pub fn with_read_key(mut self, read_key: String) -> AppState {
        self.read_key = read_key;
        self
    }

    /// Where the daily report fetches Pro fleet numbers. Must be loopback or
    /// `pro_stats_allowed_host` — see `pro_stats_url_allowed`.
    pub fn with_pro_stats_url(mut self, pro_stats_url: String) -> AppState {
        self.pro_stats_url = pro_stats_url;
        self
    }

    /// The one non-loopback host `pro_stats_url` may point at.
    pub fn with_pro_stats_allowed_host(mut self, host: String) -> AppState {
        self.pro_stats_allowed_host = host;
        self
    }

    /// Build state from env: `APEX_ADMIN_KEY`, `APEX_READ_KEY` (each falling
    /// back to its pre-rename `DOTTIE_*` name),
    /// `DOTTIE_LATEST_VERSION` (default "7.99"), `DOTTIE_MIN_VERSION` (default
    /// "7.99"), `DOTTIE_DOWNLOAD_URL` (default the GitHub releases URL), plus
    /// `GITHUB_TOKEN` / `GITHUB_REPOS` for the growth-chart merge. Missing
    /// keys default to "" — which disables that guard's key branch entirely (an
    /// empty key never matches), exactly like the JS `?? ""`.
    pub fn from_env(db: Db) -> AppState {
        let admin_key = env_first(&["APEX_ADMIN_KEY", "DOTTIE_ADMIN_KEY"]);
        let read_key = env_first(&["APEX_READ_KEY", "DOTTIE_READ_KEY"]);
        let version = VersionConfig {
            latest: std::env::var("DOTTIE_LATEST_VERSION").unwrap_or_else(|_| "7.99".to_string()),
            min: std::env::var("DOTTIE_MIN_VERSION").unwrap_or_else(|_| "7.99".to_string()),
            provider_models: ["openai", "anthropic", "cerebras"]
                .iter()
                .map(|p| {
                    let key = format!("BYOK_MODELS_{}", p.to_uppercase());
                    let raw = std::env::var(&key)
                        .unwrap_or_else(|_| crate::models::default_byok_models(p).to_string());
                    (p.to_string(), crate::models::parse_models_or(&raw, crate::models::default_byok_models(p)))
                })
                .collect(),
            vision_model: env_or("VISION_MODEL", "grok-4.3"),
            local_model: env_or("LOCAL_MODEL", "ggml-org/gemma-4-E4B-it-GGUF"),
            local_model_high_mem: env_or("LOCAL_MODEL_HIGH_MEM", "ggml-org/gemma-4-12B-it-GGUF"),
            stt_model_url_base: env_or(
                "STT_MODEL_URL_BASE",
                "https://huggingface.co/mudler/parakeet-cpp-gguf/resolve/main",
            ),
            default_provider: std::env::var("DOTTIE_DEFAULT_PROVIDER")
                .ok()
                .filter(|v| !v.is_empty())
                .unwrap_or_else(|| "dottiepro".to_string()),
            download_url: std::env::var("DOTTIE_DOWNLOAD_URL").unwrap_or_else(|_| {
                // Verbatim default from server.js — must match so /api/version
                // returns an identical body when the env var is unset.
                "https://github.com/stevederico/dottie-desktop-releases/releases/latest/download/Dottie.zip"
                    .to_string()
            }),
        };
        let mut state = AppState::new(db, admin_key, version)
            .with_read_key(read_key)
            .with_github(GithubConfig::from_env())
            .with_pro_stats_url(std::env::var("PRO_STATS_URL").unwrap_or_default().trim().to_string())
            .with_pro_stats_allowed_host(
                std::env::var("PRO_STATS_ALLOWED_HOST")
                    .unwrap_or_default()
                    .trim()
                    .to_ascii_lowercase(),
            )
            .with_landing_origin(normalize_origin(
                &std::env::var("LANDING_ALLOWED_ORIGIN").unwrap_or_default(),
            ));
        // Both features fail closed, so say so at boot rather than silently.
        if state.landing_origin.is_empty() {
            tracing::warn!("LANDING_ALLOWED_ORIGIN unset: /api/landing-visit and /api/landing-event refuse every request");
        } else if state.landing_origin.contains(',') {
            tracing::warn!("LANDING_ALLOWED_ORIGIN holds a comma: it is one origin, not a list, so nothing will match");
        }
        if state.pro_stats_url.is_empty() {
            tracing::warn!("PRO_STATS_URL unset: daily report Pro fleet and refund sections print unavailable");
        } else if !crate::routes::pro_stats_url_allowed(&state.pro_stats_url, &state.pro_stats_allowed_host) {
            tracing::warn!("PRO_STATS_URL host is not loopback or PRO_STATS_ALLOWED_HOST: the Pro stats fetch is skipped");
        }
        state.pro_models = crate::models::parse_pro_models(
            &std::env::var("PRO_MODELS").unwrap_or_default(),
        );
        state
    }
}

/// A 404 with an empty body (the JS `c.body(null, 404)` shape) — used for `/`.
async fn body_404() -> Response {
    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::empty())
        .expect("static 404 builds")
}

async fn health() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"status":"ok"}"#))
        .expect("static health builds")
}

async fn robots() -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(Body::from("User-agent: *\nDisallow: /\n"))
        .expect("static robots builds")
}

/// Canonical form of a configured origin: trimmed, lowercased, no trailing
/// `/`. Browsers send `Origin` as lowercase scheme://host[:port], so this is
/// what an exact compare needs. Empty stays empty (feature off).
pub fn normalize_origin(raw: &str) -> String {
    raw.trim().trim_end_matches('/').to_ascii_lowercase()
}

/// Build the full application router with all app-wide layers applied.
///
/// Per-route protection is attached with `.route_layer(...)` so it runs only for
/// that route's matched requests (not on 404s for unknown paths). The auth
/// guards and the rate-limit layer stack on the protected routes; the order
/// below means rate-limit runs OUTERMOST (checked first), then the guard — i.e.
/// `ServiceBuilder` applies top-to-bottom as the request descends.
pub fn build_app(state: AppState) -> Router {
    // Desktop-protected ingest routes. Each has its OWN per-route rate limit but
    // the SAME desktop guard, so `desktop_route` factors the wiring: within a
    // `ServiceBuilder` the FIRST `.layer` is OUTERMOST, so rate-limit runs before
    // the auth guard (matching the JS `rateLimit(...), desktopKeyGuard` order).
    let desktop_route = |router: Router<AppState>, max: u32| -> Router<AppState> {
        router.route_layer(
            ServiceBuilder::new()
                .layer(axum::middleware::from_fn_with_state(
                    (state.clone(), RateLimit { max, window_ms: 60_000 }),
                    rate_limit_mw,
                ))
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    desktop_bearer_guard,
                )),
        )
    };

    let events = desktop_route(
        Router::new()
            .route("/api/events", post(routes::events))
            .route_layer(DefaultBodyLimit::max(MAX_EVENT_BODY_BYTES)),
        120,
    );
    let ping = desktop_route(Router::new().route("/api/ping", post(routes::ping)), 60);

    // Batch ingest (desktop key; rate 30). ServiceBuilder applies top-to-bottom
    // as the request descends, so: rate-limit → desktop guard → body limit →
    // gzip decompression (INNERMOST). Decompression must sit below the `from_fn`
    // guards because those are typed to axum's `Body`; only the handler sees the
    // decompressed body. The `DefaultBodyLimit` is enforced by the `Bytes`
    // extractor at read time on the DECOMPRESSED body, so it also bounds a
    // gzip-bombed payload (not just the compressed input).
    let events_batch = Router::new()
        .route("/api/events/batch", post(routes::events_batch))
        .route_layer(
            ServiceBuilder::new()
                .layer(axum::middleware::from_fn_with_state(
                    (state.clone(), RateLimit { max: 30, window_ms: 60_000 }),
                    rate_limit_mw,
                ))
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    desktop_bearer_guard,
                ))
                .layer(DefaultBodyLimit::max(MAX_BATCH_BODY_BYTES))
                .layer(RequestDecompressionLayer::new()),
        );
    let diagnostics = desktop_route(
        Router::new().route("/api/diagnostics", post(routes::diagnostics)),
        5,
    );

    // Public register route: NO auth, rate 10/min.
    let register = Router::new()
        .route("/api/register", post(routes::register))
        .route_layer(axum::middleware::from_fn_with_state(
            (state.clone(), RateLimit { max: 10, window_ms: 60_000 }),
            rate_limit_mw,
        ));

    // Public version route: NO auth, rate 60/min.
    let version = Router::new()
        .route("/api/version", get(routes::version))
        .route_layer(axum::middleware::from_fn_with_state(
            (state.clone(), RateLimit { max: 60, window_ms: 60_000 }),
            rate_limit_mw,
        ));

    // Admin-protected report routes: all 11 `/api/stats/*` + `/api/diagnostics/*`
    // GETs share the SAME admin guard + rate limit. The `.route_layer` runs the
    // stack only for these matched routes, so unknown paths still 404 without
    // touching the guard.
    //
    // Limit is 240/min (was 30). The key is `{path}:{ip}`, so ONE dashboard
    // interaction bursts many hits to a single path bucket: picking a range
    // re-runs Overview's KPI summary, firing 5 `/api/stats/series` calls
    // (dau/events/pings/signups/errors) + the chart's = 6 to `/api/stats/series`
    // at once. 30/min tripped 429 after ~5 range changes. These routes are
    // bearer-gated (wrong key → 404) and single-operator, so a low cap bought
    // nothing; 240/min (4 req/s/path/ip) still caps DoS while fitting real use.
    let admin_protected = Router::new()
        .route("/api/stats", get(routes::stats))
        .route("/api/stats/errors", get(routes::stats_errors))
        .route("/api/stats/retention", get(routes::stats_retention))
        .route("/api/stats/active", get(routes::stats_active))
        .route("/api/stats/series", get(routes::stats_series))
        .route("/api/stats/top-events", get(routes::stats_top_events))
        .route("/api/stats/active-users", get(routes::stats_active_users))
        .route(
            "/api/stats/errors-by-version",
            get(routes::stats_errors_by_version),
        )
        .route(
            "/api/stats/error-signatures",
            get(routes::stats_error_signatures),
        )
        .route("/api/stats/activation", get(routes::stats_activation))
        .route("/api/stats/funnel", get(routes::stats_funnel))
        .route("/api/stats/crashes", get(routes::stats_crashes))
        .route("/api/stats/new-errors", get(routes::stats_new_errors))
        .route("/api/stats/event-details", get(routes::stats_event_details))
        // Phase 1 report endpoints (same admin guard + 30/min).
        .route("/api/stats/issues", get(routes::stats_issues))
        .route("/api/stats/lifecycle", get(routes::stats_lifecycle))
        .route("/api/stats/attribution", get(routes::stats_attribution))
        .route("/api/stats/breakdown", get(routes::stats_breakdown))
        .route("/api/stats/users", get(routes::stats_users))
        // In-app feedback triage (src/feedback.rs). GETs take the read key;
        // the status change is a write → admin key. POST alias for Cloudflare.
        .route("/api/stats/feedback", get(feedback::stats_feedback))
        .route(
            "/api/feedback/{id}",
            patch(feedback::patch_feedback).post(feedback::patch_feedback),
        )
        .route("/api/feedback/{id}/screenshot", get(feedback::feedback_screenshot))
        // Operator routes live under /api/ops, NEVER /api/admin: the Cloudflare
        // custom rule blocks any path CONTAINING "admin" (unanchored), so
        // /api/admin/* 403s with challenge HTML at the edge and the admin key
        // never reaches this router. Same guard + rate limit as /api/stats/*.
        // Operator free-credit wipe (dev Mac burned trial). POST only → admin key.
        .route("/api/ops/registrations", get(routes::ops_registrations))
        // Landing Umami-mirror reports (same admin guard + 30/min).
        .route(
            "/api/stats/landing-events",
            get(routes::stats_landing_events),
        )
        .route(
            "/api/stats/landing-funnel",
            get(routes::stats_landing_funnel),
        )
        // Multi-tenant site analytics reads (same admin guard + 30/min).
        .route("/api/sites", get(routes::list_sites).post(routes::create_site))
        .route("/api/site/summary", get(routes::site_summary))
        .route("/api/site/series", get(routes::site_series))
        .route("/api/site/events", get(routes::site_events))
        .route("/api/site/pages", get(routes::site_pages))
        .route("/api/site/referrers", get(routes::site_referrers))
        .route("/api/site/breakdown", get(routes::site_breakdown))
        .route("/api/site/user-agents", get(routes::site_user_agents))
        // POST alias for the same handler: Cloudflare's WAF challenges PATCH from
        // non-browser clients (curl gets the 403 "Attention Required" page even
        // with the Dottie UA + admin key; verified 2026-07-10 — POST reaches the
        // origin, PATCH never does). CLI/agent workflows resolve issues via POST.
        // Same body ({ "status": "resolved" | "active" }), same guard + rate limit.
        .route("/api/issues/{id}", patch(routes::patch_issue).post(routes::patch_issue))
        // Admin purge of one visitor's landing data (events + visit). POST alias
        // for Cloudflare (same DELETE/PATCH challenge as the issues route).
        .route(
            "/api/landing/{iid}",
            delete(routes::delete_landing).post(routes::delete_landing),
        )
        .route("/api/diagnostics/list", get(routes::diagnostics_list))
        .route("/api/diagnostics/{id}", get(routes::diagnostics_detail))
        // Growth (downloads/metrics/repos) — admin-gated like all other report APIs.
        // Wrong/missing key → 404 (probe-resistant), same as stats/sites.
        .route("/api/downloads", get(routes::growth_downloads))
        .route("/api/downloads/daily", get(routes::growth_downloads_daily))
        .route("/api/downloads/latest", get(routes::growth_downloads_latest))
        .route("/api/downloads/wow", get(routes::growth_downloads_wow))
        .route("/api/downloads/repos", get(routes::growth_downloads_repos))
        .route("/api/downloads/snapshot", post(routes::growth_downloads_snapshot))
        .route("/api/downloads/backfill", post(routes::growth_downloads_backfill))
        .route("/api/metrics", get(routes::growth_metrics))
        .route("/api/metrics/daily", get(routes::growth_metrics_daily))
        .route("/api/metrics/latest", get(routes::growth_metrics_latest))
        .route("/api/metrics/wow", get(routes::growth_metrics_wow))
        .route("/api/metrics/snapshot", post(routes::growth_metrics_snapshot))
        .route("/api/repos", get(routes::growth_list_repos).post(routes::growth_add_repo))
        // POST alias for kind update — Cloudflare WAF challenges PATCH from non-browsers.
        .route(
            "/api/repos/{id}",
            delete(routes::growth_delete_repo)
                .patch(routes::growth_patch_repo)
                .post(routes::growth_patch_repo),
        )
        .route_layer(
            ServiceBuilder::new()
                .layer(axum::middleware::from_fn_with_state(
                    (state.clone(), RateLimit { max: 240, window_ms: 60_000 }),
                    rate_limit_mw,
                ))
                // Method-aware: read_key unlocks the GETs here; POSTs on this
                // router (create_site, snapshot, backfill, add_repo, patch/delete)
                // still require admin_key.
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    report_key_guard,
                )),
        );

    // Report composites (browser-friendly MD). Own router so they can also
    // accept the read_key via a `?key=` query param — for status pages / webhooks
    // that fetch a plain URL with no Authorization header. Header admin/read key
    // still works; the query path is read_key-only. 30/min, same 404-on-miss.
    let report_public = Router::new()
        .route("/api/report/daily", get(routes::report_daily))
        .route("/api/report/site-daily", get(routes::report_site_daily))
        .route_layer(
            ServiceBuilder::new()
                .layer(axum::middleware::from_fn_with_state(
                    (state.clone(), RateLimit { max: 30, window_ms: 60_000 }),
                    rate_limit_mw,
                ))
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    report_public_guard,
                )),
        );

    // Growth-chart history import: larger body, same admin guard, 30/min.
    let growth_import = Router::new()
        .route(
            "/api/import/github-growth",
            post(routes::growth_import),
        )
        .route_layer(
            ServiceBuilder::new()
                .layer(axum::middleware::from_fn_with_state(
                    (state.clone(), RateLimit { max: 30, window_ms: 60_000 }),
                    rate_limit_mw,
                ))
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    admin_key_guard,
                ))
                .layer(DefaultBodyLimit::max(8 * 1024 * 1024)),
        );

    // Admin bulk-import for multi-tenant backfill. Own router: same admin guard,
    // but a higher 300/min limit and an 8 MiB body cap (a 2000-event batch with
    // props is far larger than the 256 KiB desktop-batch cap), gzip-decompressed
    // like events/batch. ServiceBuilder top-to-bottom: rate-limit → admin guard →
    // body limit → decompression (innermost).
    let site_import = Router::new()
        .route("/api/import/site-events", post(routes::import_site_events))
        .route("/api/import/feedback", post(feedback::import_feedback))
        .route_layer(
            ServiceBuilder::new()
                .layer(axum::middleware::from_fn_with_state(
                    (state.clone(), RateLimit { max: 300, window_ms: 60_000 }),
                    rate_limit_mw,
                ))
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    admin_key_guard,
                ))
                .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
                .layer(RequestDecompressionLayer::new()),
        );

    // Public landing route: own rate limit, no desktop auth. CORS is enforced by
    // the HANDLERS, not a tower CorsLayer — the layer intercepts every OPTIONS as
    // a preflight and answers with 200, which would shadow our explicit
    // `landing_visit_options` (server.js returns 204 + CORS headers). Both the
    // POST and OPTIONS handlers check the Origin server-side (403 on mismatch),
    // set ACAO on success, and the OPTIONS handler returns the 204 preflight —
    // an exact port of the JS `app.options(...)` + `c.header(ACAO, origin)`.
    let landing = Router::new()
        .route(
            "/api/landing-visit",
            post(routes::landing_visit)
                .on(MethodFilter::OPTIONS, routes::landing_visit_options),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            (state.clone(), RateLimit { max: 60, window_ms: 60_000 }),
            rate_limit_mw,
        ));

    // Named landing events (Umami mirror): same public/CORS posture as
    // landing-visit but a higher 120/min limit — one active visitor fires many
    // events (scroll-depth, section-view, time-on-page, ...) per pageview. OPTIONS
    // reuses `landing_visit_options` (origin-checked 204 + CORS, method-agnostic).
    let landing_event = Router::new()
        .route(
            "/api/landing-event",
            post(routes::landing_event)
                .on(MethodFilter::OPTIONS, routes::landing_visit_options),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            (state.clone(), RateLimit { max: 120, window_ms: 60_000 }),
            rate_limit_mw,
        ));

    // Live web-analytics ingest (Phase B): public, keyed by each site's
    // write_key in the body (NOT desktop/admin auth), high 240/min limit (one
    // active visitor fires a pageview + several custom events). CORS is handled
    // IN the handlers (reflect any registered site origin) — no tower CorsLayer,
    // so "no blanket CORS" stays literally true at the layer level. Unknown
    // write_key → 404, bot UAs stored with `bot = 1` and answered 204.
    // `/api/send` = Umami-shaped path (filter lists often block `*collect*`);
    // `/api/collect` kept as alias for older trackers.
    let collect = Router::new()
        .route(
            "/api/send",
            post(routes::collect).on(MethodFilter::OPTIONS, routes::collect_options),
        )
        .route(
            "/api/collect",
            post(routes::collect).on(MethodFilter::OPTIONS, routes::collect_options),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            (state.clone(), RateLimit { max: 240, window_ms: 60_000 }),
            rate_limit_mw,
        ));

    // Feedback widget ingest (src/feedback.rs): public, keyed by the site's
    // write_key like collect, CORS reflected in-handler. 20/min per IP is far
    // above human typing speed; a per-site daily budget backs it. Body cap is
    // route-scoped (2 MiB screenshot + fields) so every other route keeps 4 KiB.
    let feedback_ingest = Router::new()
        .route(
            "/api/feedback",
            post(feedback::feedback_ingest).on(MethodFilter::OPTIONS, feedback::feedback_options),
        )
        // Retired feedback-assistant API (src/feedback_legacy.rs): old embeds
        // and direct `/v1` callers, forwarded here by dottie-proxy.
        .route(
            "/v1/submissions",
            post(feedback_legacy::v1_submission).on(MethodFilter::OPTIONS, feedback_legacy::v1_options),
        )
        .route(
            "/v1/screenshots",
            post(feedback_legacy::v1_screenshot).on(MethodFilter::OPTIONS, feedback_legacy::v1_options),
        )
        .route_layer(
            ServiceBuilder::new()
                .layer(axum::middleware::from_fn_with_state(
                    (state.clone(), RateLimit { max: 20, window_ms: 60_000 }),
                    rate_limit_mw,
                ))
                .layer(DefaultBodyLimit::max(feedback::MAX_FEEDBACK_BODY_BYTES)),
        );

    // Dashboard SPA → gRPC-Web (tonic Routes → axum). REST admin routes above
    // stay for scripts/CLI/desktop; only the browser UI switches to this path.
    // Built as a separate Router (no AppState) so it merges after REST
    // `.with_state` — tonic's router is `Router<()>`, not `Router<AppState>`.
    let grpc_admin = TonicRoutes::default()
        .add_service(grpc::admin_grpc_service(state.clone()))
        .into_axum_router()
        .layer(tonic_web::GrpcWebLayer::new());

    let rest = Router::new()
        .route("/health", get(health))
        .route("/robots.txt", get(robots))
        // Public tracker script sites embed (Phase B). Carries no secret — the
        // per-site write_key lives in each site's <script data-website-id> tag.
        .route("/script.js", get(routes::script_js))
        // Feedback widget (one tag, same data-website-id) + its lazy screenshot lib.
        .route("/feedback.js", get(feedback::widget_js))
        .route("/feedback/html2canvas-v2.js", get(feedback::widget_html2canvas))
        .route("/feedback/html2canvas-v1.js", get(feedback::widget_html2canvas_v1))
        // Old feedback-assistant widget URLs → the current widget (legacy shim).
        .route("/widget.js", get(feedback_legacy::widget_js))
        .route("/widget/{file}", get(feedback_legacy::widget_path))
        .route(
            "/v1/projects/{key}/{what}",
            get(feedback_legacy::v1_project).on(MethodFilter::OPTIONS, feedback_legacy::v1_options),
        )
        // Phase 2 admin dashboard — public page shell; its data is admin-gated.
        // Served at /dashboard (NOT /admin — Cloudflare's WAF blocks /admin as a
        // common attack path with a 403 challenge before it reaches the origin).
        .route("/dashboard", get(routes::dashboard))
        // The dashboard is now a Vite SPA; its hashed JS/CSS/font assets live
        // under /dashboard/assets/* (embedded in the binary, served here).
        .route("/dashboard/{*path}", get(routes::dashboard_asset))
        // Public API docs (static single-file page + icon/favicon/llms.txt),
        // embedded from `docs/`. Documents the API
        // but calls nothing, so serving it unauthenticated leaks nothing.
        .route("/docs", get(routes::docs))
        .route("/docs/{*path}", get(routes::docs_asset))
        .route("/", get(body_404))
        .merge(events)
        .merge(events_batch)
        .merge(ping)
        .merge(diagnostics)
        .merge(register)
        .merge(version)
        .merge(admin_protected)
        .merge(report_public)
        .merge(site_import)
        .merge(landing)
        .merge(landing_event)
        .merge(collect)
        .merge(feedback_ingest)
        // 4 KiB request-body cap (port of JS `MAX_BODY_BYTES`) → 413 on exceed.
        // REST-only — not applied to gRPC-Web or bulk import routes below.
        .layer(DefaultBodyLimit::max(MAX_BODY_BYTES))
        // 15s timeout is REST-only. Server-streaming gRPC must stay open.
        .layer(TimeoutLayer::with_status_code(
            StatusCode::REQUEST_TIMEOUT,
            Duration::from_secs(15),
        ))
        .with_state(state.clone());

    // Growth history import has its own 8 MiB limit — merge outside the 4 KiB cap.
    // Off-box DB backup (see backup.rs). Own router merged OUTSIDE the 15s REST
    // timeout: VACUUM INTO on a large DB can outlast it. Admin key only (404
    // otherwise), 6/min. Path avoids every Cloudflare WAF `contains` word
    // (admin, backup, debug, config, ...): `/db/backup` 403'd at the edge.
    let db_backup = Router::new()
        .route("/api/ops/db/snapshot", get(backup::ops_db_backup))
        .route_layer(
            ServiceBuilder::new()
                .layer(axum::middleware::from_fn_with_state(
                    (state.clone(), RateLimit { max: 6, window_ms: 60_000 }),
                    rate_limit_mw,
                ))
                .layer(axum::middleware::from_fn_with_state(
                    state.clone(),
                    admin_key_guard,
                )),
        )
        .with_state(state.clone());

    let growth_import = growth_import.with_state(state);

    Router::new()
        .merge(rest)
        .merge(growth_import)
        .merge(db_backup)
        .merge(grpc_admin)
        .layer(
            // App-wide layers (REST + gRPC): noindex + tracing. No timeout here —
            // streams would die at 15s.
            ServiceBuilder::new()
                .layer(SetResponseHeaderLayer::overriding(
                    header::HeaderName::from_static("x-robots-tag"),
                    HeaderValue::from_static("noindex, nofollow, noarchive"),
                ))
                .layer(TraceLayer::new_for_http()),
        )
}

#[cfg(test)]
mod tests {
    use super::env_first;

    #[test]
    fn env_first_prefers_new_name_and_falls_back_to_old() {
        // Unique names so parallel tests never race on the same variable.
        std::env::set_var("ENV_FIRST_TEST_OLD", "old");
        assert_eq!(env_first(&["ENV_FIRST_TEST_NEW", "ENV_FIRST_TEST_OLD"]), "old");
        std::env::set_var("ENV_FIRST_TEST_NEW", "");
        assert_eq!(
            env_first(&["ENV_FIRST_TEST_NEW", "ENV_FIRST_TEST_OLD"]),
            "old",
            "an empty new value does not mask the old one"
        );
        std::env::set_var("ENV_FIRST_TEST_NEW", "new");
        assert_eq!(env_first(&["ENV_FIRST_TEST_NEW", "ENV_FIRST_TEST_OLD"]), "new");
        assert_eq!(env_first(&["ENV_FIRST_TEST_MISSING"]), "");
    }
}
