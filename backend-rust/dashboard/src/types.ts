// Response shapes for the dottie-analytics admin API. These describe our own
// Rust endpoints (src/reports.rs), so they are trusted — see the yagni note in
// api.ts. Fields are typed loosely (nullable) to match the SQL that can emit
// NULLs for absent dimensions.

export interface NameCount { name: string; count: number }
export interface CountryCount { country: string; count: number }
export interface SeriesPoint { date: string; count: number }
export interface Series { series: SeriesPoint[] }

export interface Site {
  id: string;
  name: string;
  events: number;
  domain?: string | null;
  write_key?: string;
  visitors?: number;
  last_event?: number | null;
  /** Linked GitHub owner/name when set */
  repo?: string | null;
}
export interface SitesResponse { sites: Site[] }

export interface Stats {
  users: number;
  dau: number; dau_engaged: number;
  wau: number; wau_engaged: number;
  mau: number; mau_engaged: number;
  stickiness: number | null;
  pings_24h: number;
  events_24h: number;
  top_events_24h: NameCount[];
  countries: CountryCount[];
}

export interface LifecycleWeek {
  week: string;
  new: number; returning: number; resurrecting: number; dormant: number;
}
export interface LifecycleResponse { weeks: LifecycleWeek[] }

export interface FunnelStep { name: string; installs: number }
export interface FunnelResponse { steps: FunnelStep[] }

export interface ActivationUser {
  name: string | null; email: string | null; version: string | null; country: string | null;
  activated: boolean;
  time_to_activation_ms: number | null;
  total_session_ms: number | null;
  session_count: number;
}
export interface ActivationResponse {
  total: number; activated: number; activation_rate: number | null;
  users: ActivationUser[];
}

export interface RetentionBucket { rate: number | null }
export interface Cohort {
  week: string; size: number; engaged: number;
  d1: RetentionBucket | null; d7: RetentionBucket | null; d30: RetentionBucket | null;
}
export interface RetentionResponse { cohorts: Cohort[] }

export interface Issue {
  id: number | string; status: 'active' | 'regressed' | 'resolved';
  name: string; file: string | null; function: string | null;
  /** Swift line number OR structured `provider` when fingerprint is code|reason|provider. */
  line: string | number | null;
  count: number; first_seen: string | number | null; last_seen: string | number | null;
  fingerprint?: string; last_version?: string | null; resolved_at?: string | number | null;
}
export interface IssuesResponse { rows: Issue[] }

export interface EventDetailRow { install_id: string; version: string | null; at: string; props: unknown }
export interface EventDetailsResponse { name: string; window_hours: number; rows: EventDetailRow[] }

export interface AttributionRow { source: string; medium: string; campaign: string; signups: number }
export interface AttributionResponse { total: number; rows: AttributionRow[] }

export interface LandingEventRow { name: string; count: number; visitors: number }
export interface LandingEventsResponse { total: number; rows: LandingEventRow[] }
export interface LandingFunnelStep { step: string; count: number }
export interface LandingFunnelResponse { steps: LandingFunnelStep[] }

export interface BreakdownRow { bucket: string; users: number; engaged: number }
export interface BreakdownResponse { rows: BreakdownRow[] }

export interface UserRow {
  name: string | null; email: string | null; version: string | null; country: string | null;
  signed_up_at: string | number | null; last_seen: string | number | null;
  active_days: number; msgs: number; voice: number;
}
export interface UsersResponse { total: number; rows: UserRow[] }

export interface SiteEventRow { name: string; count: number; visitors: number }
export interface SiteEventsResponse { total: number; rows: SiteEventRow[] }
export interface SitePageRow { url: string; views: number; visitors: number }
export interface SitePagesResponse { rows: SitePageRow[] }
export interface SiteReferrerRow { referrer: string; visitors: number; hits: number }
export interface SiteReferrersResponse { rows: SiteReferrerRow[] }

/** Range KPIs from GET /api/site/summary (Umami-style). bounce_rate ∈ [0,1]. */
export interface SiteSummaryWindow {
  visitors: number;
  visits: number;
  views: number;
  bounce_rate: number | null;
  visit_duration_ms: number;
  events: number;
}
export interface SiteSummary extends SiteSummaryWindow {
  prev: SiteSummaryWindow;
}

export interface SiteBreakdownRow { name: string; visitors: number; hits: number }
export interface SiteBreakdownResponse { by: string; rows: SiteBreakdownRow[] }

export type FeedbackStatus = 'new' | 'read' | 'archived';
export interface FeedbackRow {
  id: number;
  site_id: string;
  site_name: string | null;
  message: string;
  url: string | null;
  user_agent: string | null;
  app_version: string | null;
  end_user_id: string | null;
  end_user_name: string | null;
  end_user_email: string | null;
  country: string | null;
  status: FeedbackStatus;
  created_at: number;
  has_screenshot: boolean;
  screenshot_bytes: number | null;
}
export interface FeedbackResponse {
  rows: FeedbackRow[];
  counts: { total: number; new: number; read: number; archived: number };
}
