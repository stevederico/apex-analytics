//! gRPC-Web Admin service for the dashboard SPA.
//!
//! REST `/api/*` stays for desktop, scripts, and public ingest. The SPA talks
//! here over gRPC-Web (browsers cannot speak native gRPC). Responses carry the
//! same JSON shapes as the matching REST handlers (`JsonBody.json`).
//!
//! **Server streaming** (xAI-style): `StreamStats` / `StreamSeries` / … open a
//! long-lived RPC, push the first snapshot immediately, then re-poll the DB and
//! emit only when the JSON payload changes. Client abort = hangup.
//!
//! Auth: `authorization: Bearer <DOTTIE_ADMIN_KEY>` metadata — wrong/missing →
//! `NOT_FOUND` (probe-resistant, same idea as HTTP 404). Rate limit: 30/min/IP
//! under path key `grpc.admin` (once per stream open, not per tick).

use std::pin::Pin;
use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};

use crate::app::AppState;
use crate::middleware::{client_ip, safe_equal};
use crate::validate::{js_number, parse_num_query};

pub mod pb {
    tonic::include_proto!("dottie.admin.v1");
}

use pb::admin_server::{Admin, AdminServer};
use pb::*;

/// Path key for the rate limiter (one bucket for all admin RPCs, like REST admin).
const GRPC_ADMIN_PATH: &str = "grpc.admin";
const RATE_MAX: u32 = 30;
const RATE_WINDOW_MS: u64 = 60_000;

/// Default / clamp for stream poll interval (ms).
fn clamp_interval_ms(raw: u32) -> u64 {
    if raw == 0 {
        3_000
    } else {
        u64::from(raw).clamp(1_000, 60_000)
    }
}

type JsonStream = Pin<Box<dyn Stream<Item = Result<JsonBody, Status>> + Send>>;

/// Poll `fetch` forever: emit first snapshot, then only on JSON change.
/// Channel closes when the client drops the stream.
fn change_stream<F, Fut>(interval_ms: u64, mut fetch: F) -> JsonStream
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = Result<Value, Status>> + Send + 'static,
{
    let (tx, rx) = mpsc::channel::<Result<JsonBody, Status>>(4);
    tokio::spawn(async move {
        let mut last = String::new();
        loop {
            match fetch().await {
                Ok(v) => {
                    let json = v.to_string();
                    if json != last {
                        last = json.clone();
                        if tx.send(Ok(JsonBody { json })).await.is_err() {
                            break;
                        }
                    }
                }
                Err(e) => {
                    let _ = tx.send(Err(e)).await;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(interval_ms)).await;
        }
    });
    Box::pin(ReceiverStream::new(rx))
}

#[derive(Clone)]
pub struct AdminSvc {
    state: AppState,
}

impl AdminSvc {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }

    /// Bearer admin key + 30/min rate limit. Uses request metadata (HTTP headers
    /// on gRPC-Web). Failures are NOT_FOUND / RESOURCE_EXHAUSTED.
    fn gate<T>(&self, req: &Request<T>) -> Result<(), Status> {
        let md = req.metadata();
        let auth = md
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let provided = auth.strip_prefix("Bearer ").unwrap_or("");
        if self.state.admin_key.is_empty()
            || !safe_equal(provided.as_bytes(), self.state.admin_key.as_bytes())
        {
            return Err(Status::not_found("not_found"));
        }

        // Rebuild a HeaderMap for client_ip (same XFF / X-Real-IP rules as REST).
        let mut headers = axum::http::HeaderMap::new();
        for key in ["x-forwarded-for", "x-real-ip"] {
            if let Some(v) = md.get(key).and_then(|v| v.to_str().ok()) {
                if let Ok(name) = axum::http::HeaderName::from_bytes(key.as_bytes()) {
                    if let Ok(val) = axum::http::HeaderValue::from_str(v) {
                        headers.insert(name, val);
                    }
                }
            }
        }
        let ip = client_ip(&headers);
        if !self
            .state
            .rate_limiter
            .check(GRPC_ADMIN_PATH, &ip, RATE_MAX, RATE_WINDOW_MS)
        {
            return Err(Status::resource_exhausted("rate_limited"));
        }
        Ok(())
    }
}

fn opt_str(s: &str) -> Option<&str> {
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

fn json_ok(v: Value) -> Result<Response<JsonBody>, Status> {
    Ok(Response::new(JsonBody {
        json: v.to_string(),
    }))
}

fn db_err(what: &str, e: sqlx::Error) -> Status {
    tracing::error!("{what} failed: {e}");
    Status::internal("server_error")
}

fn site_window(days: &str, from: &str, to: &str, hours: &str) -> (i64, i64) {
    if !from.is_empty() && !to.is_empty() {
        return crate::reports::resolve_series_range(None, Some(from), Some(to));
    }
    if !days.is_empty() {
        return crate::reports::resolve_series_range(Some(days), None, None);
    }
    let h = parse_num_query(opt_str(hours), 720.0).clamp(1.0, 8760.0);
    let d = format!("{}", (h / 24.0).ceil().clamp(1.0, 180.0) as i64);
    crate::reports::resolve_series_range(Some(d.as_str()), None, None)
}

#[tonic::async_trait]
impl Admin for AdminSvc {
    async fn get_stats(&self, req: Request<Empty>) -> Result<Response<JsonBody>, Status> {
        self.gate(&req)?;
        let v = self
            .state
            .db
            .get_stats()
            .await
            .map_err(|e| db_err("getStats", e))?;
        json_ok(v)
    }

    async fn get_series(&self, req: Request<SeriesRequest>) -> Result<Response<JsonBody>, Status> {
        self.gate(&req)?;
        let r = req.into_inner();
        if !matches!(
            r.metric.as_str(),
            "dau" | "events" | "errors" | "signups" | "pings"
        ) {
            return Err(Status::invalid_argument("bad_metric"));
        }
        let (since, until) = crate::reports::resolve_series_range(
            opt_str(&r.days),
            opt_str(&r.from),
            opt_str(&r.to),
        );
        let v = self
            .state
            .db
            .get_series(&r.metric, since, until)
            .await
            .map_err(|e| db_err("getSeries", e))?;
        json_ok(v)
    }

    async fn get_retention(
        &self,
        req: Request<WeeksRequest>,
    ) -> Result<Response<JsonBody>, Status> {
        self.gate(&req)?;
        let weeks = parse_num_query(opt_str(&req.into_inner().weeks), 8.0);
        let v = self
            .state
            .db
            .get_retention(weeks)
            .await
            .map_err(|e| db_err("getRetention", e))?;
        json_ok(v)
    }

    async fn get_activation(
        &self,
        req: Request<HoursRequest>,
    ) -> Result<Response<JsonBody>, Status> {
        self.gate(&req)?;
        let hours = parse_num_query(opt_str(&req.into_inner().hours), 24.0);
        let v = self
            .state
            .db
            .get_activation_funnel(hours)
            .await
            .map_err(|e| db_err("getActivationFunnel", e))?;
        json_ok(v)
    }

    async fn get_funnel(&self, req: Request<HoursRequest>) -> Result<Response<JsonBody>, Status> {
        self.gate(&req)?;
        let hours = parse_num_query(opt_str(&req.into_inner().hours), 24.0);
        let v = self
            .state
            .db
            .get_funnel_report(hours)
            .await
            .map_err(|e| db_err("getFunnelReport", e))?;
        json_ok(v)
    }

    async fn get_lifecycle(
        &self,
        req: Request<WeeksRequest>,
    ) -> Result<Response<JsonBody>, Status> {
        self.gate(&req)?;
        let weeks = parse_num_query(opt_str(&req.into_inner().weeks), 12.0);
        let v = self
            .state
            .db
            .get_lifecycle(weeks)
            .await
            .map_err(|e| db_err("getLifecycle", e))?;
        json_ok(v)
    }

    async fn get_attribution(
        &self,
        req: Request<HoursRequest>,
    ) -> Result<Response<JsonBody>, Status> {
        self.gate(&req)?;
        let hours = parse_num_query(opt_str(&req.into_inner().hours), 720.0);
        let v = self
            .state
            .db
            .get_attribution(hours)
            .await
            .map_err(|e| db_err("getAttribution", e))?;
        json_ok(v)
    }

    async fn get_breakdown(
        &self,
        req: Request<BreakdownRequest>,
    ) -> Result<Response<JsonBody>, Status> {
        self.gate(&req)?;
        let by = req.into_inner().by;
        let by = if by.is_empty() { "version" } else { by.as_str() };
        if !matches!(by, "version" | "os" | "country") {
            return Err(Status::invalid_argument("invalid_by"));
        }
        let v = self
            .state
            .db
            .get_breakdown(by)
            .await
            .map_err(|e| db_err("getBreakdown", e))?;
        json_ok(v)
    }

    async fn get_users(&self, req: Request<UsersRequest>) -> Result<Response<JsonBody>, Status> {
        self.gate(&req)?;
        let limit = parse_num_query(opt_str(&req.into_inner().limit), 200.0);
        let v = self
            .state
            .db
            .get_users_list(limit)
            .await
            .map_err(|e| db_err("getUsersList", e))?;
        json_ok(v)
    }

    async fn get_issues(&self, req: Request<IssuesRequest>) -> Result<Response<JsonBody>, Status> {
        self.gate(&req)?;
        let inner = req.into_inner();
        let status = opt_str(&inner.status);
        let v = self
            .state
            .db
            .get_issues(status)
            .await
            .map_err(|e| db_err("getIssues", e))?;
        json_ok(v)
    }

    async fn patch_issue(
        &self,
        req: Request<PatchIssueRequest>,
    ) -> Result<Response<JsonBody>, Status> {
        self.gate(&req)?;
        let r = req.into_inner();
        if r.id < 1 {
            return Err(Status::not_found("not_found"));
        }
        // Mirror REST numeric guard (js_number on path string).
        let id_s = r.id.to_string();
        let n = js_number(&id_s);
        if !(n.is_finite() && n.fract() == 0.0 && n >= 1.0 && n <= i64::MAX as f64) {
            return Err(Status::not_found("not_found"));
        }
        let status = r.status.as_str();
        if status != "active" && status != "resolved" {
            return Err(Status::invalid_argument("invalid_status"));
        }
        let version = opt_str(&r.version).filter(|v| !v.trim().is_empty());
        match self.state.db.set_issue_status(r.id, status, version).await {
            Ok(Some(v)) => json_ok(v),
            Ok(None) => Err(Status::not_found("not_found")),
            Err(e) => Err(db_err("setIssueStatus", e)),
        }
    }

    async fn get_landing_events(
        &self,
        req: Request<HoursRequest>,
    ) -> Result<Response<JsonBody>, Status> {
        self.gate(&req)?;
        let hours = parse_num_query(opt_str(&req.into_inner().hours), 720.0);
        let v = self
            .state
            .db
            .get_landing_events(hours)
            .await
            .map_err(|e| db_err("getLandingEvents", e))?;
        json_ok(v)
    }

    async fn get_landing_funnel(
        &self,
        req: Request<HoursRequest>,
    ) -> Result<Response<JsonBody>, Status> {
        self.gate(&req)?;
        let hours = parse_num_query(opt_str(&req.into_inner().hours), 720.0);
        let v = self
            .state
            .db
            .get_landing_funnel(hours)
            .await
            .map_err(|e| db_err("getLandingFunnel", e))?;
        json_ok(v)
    }

    async fn list_sites(&self, req: Request<Empty>) -> Result<Response<JsonBody>, Status> {
        self.gate(&req)?;
        let v = self
            .state
            .db
            .list_sites()
            .await
            .map_err(|e| db_err("listSites", e))?;
        json_ok(v)
    }

    async fn get_site_summary(
        &self,
        req: Request<SiteRangeRequest>,
    ) -> Result<Response<JsonBody>, Status> {
        self.gate(&req)?;
        let r = req.into_inner();
        if r.site.is_empty() {
            return Err(Status::invalid_argument("missing_site"));
        }
        let (since, until) =
            crate::reports::resolve_series_range(opt_str(&r.days), opt_str(&r.from), opt_str(&r.to));
        let v = self
            .state
            .db
            .get_site_summary(&r.site, since, until)
            .await
            .map_err(|e| db_err("getSiteSummary", e))?;
        json_ok(v)
    }

    async fn get_site_series(
        &self,
        req: Request<SiteSeriesRequest>,
    ) -> Result<Response<JsonBody>, Status> {
        self.gate(&req)?;
        let r = req.into_inner();
        if r.site.is_empty() {
            return Err(Status::invalid_argument("missing_site"));
        }
        let metric = if r.metric.is_empty() {
            "pageviews"
        } else {
            r.metric.as_str()
        };
        if !matches!(metric, "pageviews" | "events" | "visitors" | "visits") {
            return Err(Status::invalid_argument("bad_metric"));
        }
        let (since, until) =
            crate::reports::resolve_series_range(opt_str(&r.days), opt_str(&r.from), opt_str(&r.to));
        let v = self
            .state
            .db
            .get_site_series(&r.site, metric, since, until)
            .await
            .map_err(|e| db_err("getSiteSeries", e))?;
        json_ok(v)
    }

    async fn get_site_events(
        &self,
        req: Request<SiteEventsRequest>,
    ) -> Result<Response<JsonBody>, Status> {
        self.gate(&req)?;
        let r = req.into_inner();
        if r.site.is_empty() {
            return Err(Status::invalid_argument("missing_site"));
        }
        // Rolling `hours` window (proto has no days/from/to), as before.
        let (since, until) = crate::reports::resolve_report_window(
            None,
            None,
            None,
            opt_str(&r.hours),
            false,
            720.0,
        );
        let v = self
            .state
            .db
            .get_site_events(&r.site, since, until)
            .await
            .map_err(|e| db_err("getSiteEvents", e))?;
        json_ok(v)
    }

    async fn get_site_pages(
        &self,
        req: Request<SitePagesRequest>,
    ) -> Result<Response<JsonBody>, Status> {
        self.gate(&req)?;
        let r = req.into_inner();
        if r.site.is_empty() {
            return Err(Status::invalid_argument("missing_site"));
        }
        let kind = if r.kind.is_empty() {
            "path"
        } else {
            r.kind.as_str()
        };
        if !matches!(kind, "path" | "url" | "entry" | "exit") {
            return Err(Status::invalid_argument("bad_kind"));
        }
        let (since, until) = site_window(&r.days, &r.from, &r.to, &r.hours);
        let v = self
            .state
            .db
            .get_site_pages(&r.site, since, until, kind)
            .await
            .map_err(|e| db_err("getSitePages", e))?;
        json_ok(v)
    }

    async fn get_site_referrers(
        &self,
        req: Request<SiteRangeRequest>,
    ) -> Result<Response<JsonBody>, Status> {
        self.gate(&req)?;
        let r = req.into_inner();
        if r.site.is_empty() {
            return Err(Status::invalid_argument("missing_site"));
        }
        let (since, until) = site_window(&r.days, &r.from, &r.to, &r.hours);
        let v = self
            .state
            .db
            .get_site_referrers(&r.site, since, until)
            .await
            .map_err(|e| db_err("getSiteReferrers", e))?;
        json_ok(v)
    }

    async fn get_site_breakdown(
        &self,
        req: Request<SiteBreakdownRequest>,
    ) -> Result<Response<JsonBody>, Status> {
        self.gate(&req)?;
        let r = req.into_inner();
        if r.site.is_empty() {
            return Err(Status::invalid_argument("missing_site"));
        }
        let by = if r.by.is_empty() {
            "browser"
        } else {
            r.by.as_str()
        };
        if !matches!(by, "browser" | "os" | "device" | "country") {
            return Err(Status::invalid_argument("bad_by"));
        }
        let (since, until) = site_window(&r.days, &r.from, &r.to, &r.hours);
        let v = self
            .state
            .db
            .get_site_breakdown(&r.site, since, until, by)
            .await
            .map_err(|e| db_err("getSiteBreakdown", e))?;
        json_ok(v)
    }

    // ---- server streaming (xAI-style) ---------------------------------------

    type StreamStatsStream = JsonStream;
    type StreamSeriesStream = JsonStream;
    type StreamIssuesStream = JsonStream;
    type StreamSiteSummaryStream = JsonStream;
    type StreamSiteSeriesStream = JsonStream;

    async fn stream_stats(
        &self,
        req: Request<StreamOpts>,
    ) -> Result<Response<Self::StreamStatsStream>, Status> {
        self.gate(&req)?;
        let interval = clamp_interval_ms(req.into_inner().interval_ms);
        let db = self.state.db.clone();
        Ok(Response::new(change_stream(interval, move || {
            let db = db.clone();
            async move {
                db.get_stats()
                    .await
                    .map_err(|e| db_err("streamStats", e))
            }
        })))
    }

    async fn stream_series(
        &self,
        req: Request<SeriesStreamRequest>,
    ) -> Result<Response<Self::StreamSeriesStream>, Status> {
        self.gate(&req)?;
        let r = req.into_inner();
        if !matches!(
            r.metric.as_str(),
            "dau" | "events" | "errors" | "signups" | "pings"
        ) {
            return Err(Status::invalid_argument("bad_metric"));
        }
        let interval = clamp_interval_ms(r.interval_ms);
        let metric = r.metric;
        let (since, until) = crate::reports::resolve_series_range(
            opt_str(&r.days),
            opt_str(&r.from),
            opt_str(&r.to),
        );
        let db = self.state.db.clone();
        Ok(Response::new(change_stream(interval, move || {
            let db = db.clone();
            let metric = metric.clone();
            async move {
                db.get_series(&metric, since, until)
                    .await
                    .map_err(|e| db_err("streamSeries", e))
            }
        })))
    }

    async fn stream_issues(
        &self,
        req: Request<IssuesStreamRequest>,
    ) -> Result<Response<Self::StreamIssuesStream>, Status> {
        self.gate(&req)?;
        let r = req.into_inner();
        let interval = clamp_interval_ms(r.interval_ms);
        let status_filter = r.status;
        let db = self.state.db.clone();
        Ok(Response::new(change_stream(interval, move || {
            let db = db.clone();
            let status_filter = status_filter.clone();
            async move {
                let s = if status_filter.is_empty() {
                    None
                } else {
                    Some(status_filter.as_str())
                };
                db.get_issues(s)
                    .await
                    .map_err(|e| db_err("streamIssues", e))
            }
        })))
    }

    async fn stream_site_summary(
        &self,
        req: Request<SiteSummaryStreamRequest>,
    ) -> Result<Response<Self::StreamSiteSummaryStream>, Status> {
        self.gate(&req)?;
        let r = req.into_inner();
        if r.site.is_empty() {
            return Err(Status::invalid_argument("missing_site"));
        }
        let interval = clamp_interval_ms(r.interval_ms);
        let site = r.site;
        let (since, until) =
            crate::reports::resolve_series_range(opt_str(&r.days), opt_str(&r.from), opt_str(&r.to));
        let db = self.state.db.clone();
        Ok(Response::new(change_stream(interval, move || {
            let db = db.clone();
            let site = site.clone();
            async move {
                db.get_site_summary(&site, since, until)
                    .await
                    .map_err(|e| db_err("streamSiteSummary", e))
            }
        })))
    }

    async fn stream_site_series(
        &self,
        req: Request<SiteSeriesStreamRequest>,
    ) -> Result<Response<Self::StreamSiteSeriesStream>, Status> {
        self.gate(&req)?;
        let r = req.into_inner();
        if r.site.is_empty() {
            return Err(Status::invalid_argument("missing_site"));
        }
        let metric = if r.metric.is_empty() {
            "pageviews".to_string()
        } else {
            r.metric
        };
        if !matches!(
            metric.as_str(),
            "pageviews" | "events" | "visitors" | "visits"
        ) {
            return Err(Status::invalid_argument("bad_metric"));
        }
        let interval = clamp_interval_ms(r.interval_ms);
        let site = r.site;
        let (since, until) =
            crate::reports::resolve_series_range(opt_str(&r.days), opt_str(&r.from), opt_str(&r.to));
        let db = self.state.db.clone();
        Ok(Response::new(change_stream(interval, move || {
            let db = db.clone();
            let site = site.clone();
            let metric = metric.clone();
            async move {
                db.get_site_series(&site, &metric, since, until)
                    .await
                    .map_err(|e| db_err("streamSiteSeries", e))
            }
        })))
    }
}

/// Build the tonic Admin service (caller applies `GrpcWebLayer`).
pub fn admin_grpc_service(state: AppState) -> AdminServer<AdminSvc> {
    AdminServer::new(AdminSvc::new(state))
}
