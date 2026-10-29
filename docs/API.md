# apex-analytics API reference

Base URL: your deployment (`http://localhost:8080` when self-hosting). Generated from `backend-rust/src/app.rs`.

## Auth tiers

- **Public** — no auth. Rate-limited only.
- **Desktop bearer** — `Authorization: Bearer <user token>` from `/api/register` (app writes). The old shared `X-Dottie-Client` key is retired → 404.
- **Write key** — per-site `write_key` in the request body (`/api/send` / `/api/collect` and `/api/feedback`).
- **Admin** — `Authorization: Bearer <APEX_ADMIN_KEY>` (all reads/reports and every write).
- **Read key** (optional) — `Authorization: Bearer <APEX_READ_KEY>` on admin **GET** routes only. Never unlocks POST/PATCH/DELETE, imports, or the dashboard/gRPC path. Empty → off.
- **Report query key** — `/api/report/daily` and `/api/report/site-daily` also accept `?key=<APEX_READ_KEY>` (GET only). The admin key is rejected in the query string.

Probe resistance: missing/wrong key returns **404**, not 401/403. No blanket CORS; timing-safe key comparisons.

Rates are per route path per IP: each concrete path (`/api/feedback/1`, `/api/feedback/2`) has its own bucket.

## Public (no auth)

| Method | Path | Rate | Purpose |
|---|---|---|---|
| GET | `/health` | — | Healthcheck |
| GET | `/robots.txt` | — | Disallow all |
| GET | `/` | — | 404 by design (no HTML at root) |
| GET | `/script.js` | — | Tracker script sites embed; carries no secret |
| GET | `/feedback.js` | — | Feedback widget sites embed (same `data-website-id` write key) |
| GET | `/feedback/html2canvas-v1.js` | — | Screenshot library the widget lazy-loads |
| GET | `/docs`, `/docs/*path` | — | Static API docs page, icon and `llms.txt` |
| GET | `/dashboard` | — | Admin SPA shell (data is admin-gated) |
| GET | `/dashboard/*path` | — | SPA hashed JS/CSS/font assets |
| POST | `/dottie.admin.v1.Admin/*` | 30/min | gRPC-Web Admin service used by the SPA (Bearer admin key; wrong key → gRPC NOT_FOUND). Unary Get* + server-streaming Stream* (live dashboard). REST `/api/*` unchanged. |
| POST | `/api/register` | 10/min | Upsert user by email; returns per-user bearer token |
| GET | `/api/version` | 60/min | Version probe |
| POST | `/api/send`, `/api/collect` | 240/min | Umami-compatible web-analytics ingest, keyed by the site `write_key` in `payload.website`; unknown→404; bot UA→204, stored with `bot=1` and left out of reports |
| OPTIONS | `/api/send`, `/api/collect` | 240/min | CORS preflight (handler-set, origin-reflected) |
| POST | `/api/feedback` | 20/min | Feedback widget ingest (multipart or JSON, optional JPEG/PNG screenshot ≤2 MiB). Message >5000 UTF-16 units → 413, non JPEG/PNG → 415, per-site `FEEDBACK_DAILY_BUDGET` → 429 |
| OPTIONS | `/api/feedback` | 20/min | CORS preflight (origin-reflected) |
| POST | `/api/landing-visit` | 60/min | Landing pageview mirror. Origin must equal `LANDING_ALLOWED_ORIGIN` (unset → 403) |
| OPTIONS | `/api/landing-visit` | 60/min | CORS preflight (same origin check) |
| POST | `/api/landing-event` | 120/min | Named landing events (scroll/section/time). Same origin check |

## Desktop bearer (per-user)

| Method | Path | Rate | Purpose |
|---|---|---|---|
| POST | `/api/ping` | 60/min | Install ping for DAU/MAU |
| POST | `/api/events` | 120/min | Single event ingest |
| POST | `/api/events/batch` | 30/min | Batch ≤100, gzip, event_id dedupe (256 KiB cap) |
| POST | `/api/diagnostics` | 5/min | Diagnostic bundle upload |

## Admin (Bearer) — GitHub growth (growth-chart merge), 240/min

Same auth as all other admin reads: **Bearer `APEX_ADMIN_KEY`**, missing/wrong key → **404** (not public).

| Method | Path | Purpose |
|---|---|---|
| GET | `/api/downloads` | Download snapshots (`?repo` `?from` `?to`) |
| GET | `/api/downloads/daily` | Day-over-day download deltas |
| GET | `/api/downloads/latest` | Latest snapshot + per-tag totals |
| GET | `/api/downloads/wow` | Week-over-week %. `?repo=` → that project's series; omitted → one row per project |
| GET | `/api/downloads/repos` | Tracked repo name list |
| POST | `/api/downloads/snapshot` | Trigger download snapshot (`{repo?}`) |
| POST | `/api/downloads/backfill` | Historical totals (`{date,total,repo?}`) |
| GET | `/api/metrics` | Metric rows (`?metric=stars\|forks\|clones\|views`) |
| GET | `/api/metrics/daily` | Metric day deltas |
| GET | `/api/metrics/latest` | Latest metric aggregate |
| GET | `/api/metrics/wow` | Week-over-week % (`?metric=` required). `?repo=` → series; omitted → per project |
| POST | `/api/metrics/snapshot` | Trigger metric snapshot(s) |
| GET/POST | `/api/repos` | List / add tracked GitHub repos (`?kind=`, body `{repo, kind?}`) |
| PATCH/POST | `/api/repos/{id}` | Set `kind` and/or `linked_site_id` (site uuid, `"native"`, or `""`/null to clear) |
| DELETE | `/api/repos/{id}` | Remove a tracked repo |
| POST | `/api/import/github-growth` | Bulk import growth-chart history (8 MiB, admin key only, 30/min) |

Env: `GITHUB_TOKEN` (required for clones/views traffic), `GITHUB_REPOS` (comma-separated `owner/name[:kind]` seed, used only while the table is empty; unset → nothing tracked), `GITHUB_SITE_LINKS` (comma-separated `owner/name=<site uuid or native>`, fills unset links on startup). Collectors: startup full snapshot + hourly missing-today backfill. Keep the service always-on (`railway.json` `sleepApplication: false`) so web/desktop ingest never waits on a cold start.

**Site link embed:** when `github_repos.linked_site_id` matches a site id (or `"native"` for Dottie desktop), these also return `repo` + `growth` (latest downloads/stars/forks/clones/views brief):
- `GET /api/sites`, `GET /api/site/summary`, `GET /api/report/site-daily`
- `GET /api/report/daily` (the repo linked to `native`)

## Admin (Bearer) — reports & reads, 240/min (report composites 30/min)

### Desktop telemetry
| Method | Path | Purpose |
|---|---|---|
| GET | `/api/stats` | users, dau, mau, pings_24h |
| GET | `/api/report/daily` | Composite desktop report; `?format=md` for a ready-to-send body. 30/min; also `?key=` |
| GET | `/api/report/site-daily` | All-sites roll-up; `?format=md`. 30/min; also `?key=` |
| GET | `/api/stats/errors` | Error breakdown (`?hours=`) |
| GET | `/api/stats/errors-by-version` | Errors grouped by app version |
| GET | `/api/stats/error-signatures` | Individual failure modes (version + name + code/reason/provider or file/function/line); `?hours=` default 168 |
| GET | `/api/stats/top-events` | Top event names for the window (`?days=` or `?from=&to=`) |
| GET | `/api/stats/active-users` | Distinct active installs for the window (`?days=`/`?hours=`/`?from=&to=`/`?all=`) |
| GET | `/api/stats/new-errors` | Newly-seen errors |
| GET | `/api/stats/crashes` | Crash rollup |
| GET | `/api/stats/issues` | Issues list (open/resolved/regressed) |
| GET | `/api/stats/event-details` | Per-event drilldown |
| GET | `/api/stats/retention` | Retention grid |
| GET | `/api/stats/active` | Daily-active series (`?days=`) |
| GET | `/api/stats/series` | Per-day series (from/to; span clamped ≤366 days) |
| GET | `/api/stats/activation` | Activation funnel (`?hours=`) |
| GET | `/api/stats/funnel` | Generic funnel |
| GET | `/api/stats/lifecycle` | new/returning/resurrecting/dormant |
| GET | `/api/stats/attribution` | Installs by UTM (empty until iid handoff exists) |
| GET | `/api/stats/breakdown` | `?by=version\|os\|country` |
| GET | `/api/stats/users` | User browser + `?search=` |
| GET | `/api/stats/landing-events` | Landing events report |
| GET | `/api/stats/landing-funnel` | Landing funnel |
| GET | `/api/stats/feedback` | Feedback list (screenshots not inlined) |
| GET | `/api/feedback/{id}/screenshot` | One feedback screenshot |
| PATCH/POST | `/api/feedback/{id}` | Set status `new\|read\|archived` (admin key only) |
| GET | `/api/ops/registrations` | Signups grouped by IP + recent rejected registrations (`?hours=` default 168, `?limit=` default 100) |

### Multi-tenant web analytics
| Method | Path | Purpose |
|---|---|---|
| GET | `/api/sites` | Site list incl. `write_key` + optional linked `repo`/`growth` |
| POST | `/api/sites` | Create property `{name, domain?, id?, github_repo?: "owner/name"}` → site + `write_key` (+ linked repo) |
| GET | `/api/site/summary` | Per-site KPIs + optional linked `repo`/`growth` |
| GET | `/api/site/series` | Per-site time series |
| GET | `/api/site/events` | Per-site event list |
| GET | `/api/site/pages` | Top pages (path/url/entry/exit) |
| GET | `/api/site/referrers` | Top referrers |
| GET | `/api/site/breakdown` | Environment/country breakdown |
| GET | `/api/site/user-agents` | Top raw UAs + `hits_per_visitor` (audit the bot filter) |
| POST | `/api/import/site-events` | Bulk backfill (300/min, 8 MiB, gzip). Skips a row whose `id`/`event_id` is already stored, or whose site + created_at + type + session + url + name already exists. `imported` is new rows; `skipped` is the rest. |

### Mutations & ops
| Method | Path | Purpose |
|---|---|---|
| PATCH/POST | `/api/issues/{id}` | Set status `resolved\|active` (POST alias for proxies that block PATCH) |
| GET | `/api/ops/db/snapshot` | Consistent SQLite copy of the whole DB (`VACUUM INTO`, streamed; admin only, 6/min, no 15s timeout) |
| DELETE/POST | `/api/landing/{iid}` | Purge one visitor's landing data |
| GET | `/api/diagnostics/list` | Diagnostic bundles list |
| GET | `/api/diagnostics/{id}` | One diagnostic bundle |

## Not exposed via API

- Raw SQLite / generic SQL — pre-shaped reports only. (A whole-DB copy is available via `/api/ops/db/snapshot` above.)
- CSV user export — not built.
