<div align="center">

# apex-analytics

### privacy-first analytics for your sites and apps, one rust binary and a sqlite file

</div>

<br />

## Quick Start

```bash
cd backend-rust
export APEX_ADMIN_KEY=$(openssl rand -hex 32)
cargo run
# → apex-analytics listening on :8080
```

See [Self-hosting](#self-hosting) for a full setup.

<br />

## What's Included

### **Core Endpoints**
- **POST `/api/register`** — upsert a user by email; returns a per-user bearer token. Ingest (`/api/ping`, `/api/events`) also accepts an HS256 JWT signed with `PRO_JWT_SECRET`.
- **POST `/api/ping`** — record an install ping for DAU/MAU. Requires a per-user bearer token (the old `X-Dottie-Client` shared key is retired).
- **GET `/api/stats`** — users, dau, mau, pings_24h. Requires `Authorization: Bearer` header.
- **GET `/health`** — public healthcheck.
- **GET `/robots.txt`** — disallow everything.

### **Admin (bearer-gated)**
- **Reports** — `/api/stats/{active,series,retention,activation,funnel,lifecycle,attribution,issues,breakdown,users}`, `PATCH /api/issues/:id`.
- **Multi-tenant web analytics** (Umami replacement) — `/api/sites`, `/api/site/{summary,series,events,pages,referrers,breakdown}`, `POST /api/import/site-events`.
- **Landing mirror** — `POST /api/landing-event`, `/api/stats/landing-*`, `DELETE /api/landing/:iid`.
- **Dashboard** — `/dashboard`, a React/Vite admin SPA embedded in the binary. Data loads over **gRPC-Web** (`apex.admin.v1.Admin`); REST admin routes remain for scripts/CLI.
- **Growth** — GitHub downloads + stars/forks/clones/views (growth-chart merge). Admin-only `/api/downloads*`, `/api/metrics*`, `/api/repos*` (404 without bearer, like stats). Repos can `linked_site_id` → site/native reports include a `growth` brief. Set `GITHUB_TOKEN` + `GITHUB_REPOS`.

### **Feedback Assistant**
An in-app feedback widget for any site you track. Submissions land in the same SQLite DB and dashboard as that site's analytics.
- **One tag** — `<script defer src="https://your-host/feedback.js" data-website-id="<write_key>"></script>`, the same `write_key` as `/script.js`. Optional `data-app-version`; `data-host` overrides the API origin (defaults to the script's origin).
- **JS API** — `window.ApexFeedback.init({ website, apiUrl, appVersion })`, `.identify({ id, name, email })` to attach the signed-in user, `.show()` to open the popover.
- **Screenshots** — optional, JPEG/PNG ≤ 2 MiB, stored inline as a BLOB. "Screenshot" renders the viewport with html2canvas-pro (lazy-loaded from `/feedback/html2canvas-v2.js`, no permission prompt); "Share screen" captures real pixels via `getDisplayMedia`.
- **POST `/api/feedback`** — public ingest. `multipart/form-data` (`website`, `message`, `url`, `appVersion`, `endUserId`, `endUserName`, `endUserEmail`, optional `file`) or JSON without a screenshot. CORS reflects the caller's origin; unknown key → empty 404.
- **Limits** — 20/min per IP; message > 5000 chars → 413; screenshot > 2 MiB → 413, not JPEG/PNG → 415; per-site `FEEDBACK_DAILY_BUDGET` per rolling 24h → 429.
- **Triage (admin bearer)** — `GET /api/stats/feedback?site=&status=&days=&limit=` (list + status counts), `GET /api/feedback/:id/screenshot`, `PATCH|POST /api/feedback/:id` `{status: new|read|archived}`. Also a Feedback tab in `/dashboard`.
- **Import** — `POST /api/import/feedback` (admin key only) backfills rows from another store; all-or-nothing validation, duplicates skipped, `dry_run` previews.

### **Security**
- **Per-user bearer** for app-authenticated writes.
- **Bearer token** for stats/admin reads.
- **Timing-safe comparisons** on every key check.
- **Missing/wrong key returns 404** — endpoints look nonexistent to probes.
- **No blanket CORS** — browsers can't call the core API from any origin.

### **Anti-crawler**
- **`robots.txt`** disallows all bots.
- **`X-Robots-Tag: noindex, nofollow, noarchive`** on every response.
- **No HTML at the public root** — `/` returns 404.

### **Rate Limiting**
Limits count per route path per IP (`/api/feedback/1` and `/api/feedback/2` are separate buckets).
- **10/min per IP** on `/api/register`
- **60/min per IP** on `/api/ping`
- **240/min per IP** on admin reads (`/api/stats*`, `/api/site*`, growth); **30/min** on the report composites and gRPC-Web
- Full per-route table in [`docs/API.md`](docs/API.md)

<br />

## Configuration

### Environment Variables

Formerly **dottie-analytics**. The pre-rename `DOTTIE_ADMIN_KEY`, `DOTTIE_READ_KEY` and `DOTTIE_MIRROR_KEY` names still work; the `APEX_*` name wins when both are set.

| Variable | Purpose |
|---|---|
| `APEX_ADMIN_KEY` | Your private bearer token for `/api/stats` and the admin dashboard |
| `DB_PATH` | SQLite file path (default `./databases/apex-api.db`, or an existing pre-rename `dottie-api.db`; use a mounted volume in production) |
| `PORT` | HTTP port (default `8080`) |
| `GITHUB_TOKEN` | GitHub PAT for traffic metrics (clones/views); optional for public release downloads |
| `GITHUB_REPOS` | Comma-separated `owner/name[:kind]` list seeded into the growth tracker while it is empty. Unset → nothing tracked |
| `GITHUB_SITE_LINKS` | Comma-separated `owner/name=<site uuid or native>` links, applied on startup to repos with no link |
| `LANDING_ALLOWED_ORIGIN` | The one origin allowed to call `/api/landing-visit` and `/api/landing-event` (e.g. `https://www.example.com`; trailing `/` and case are normalized; one origin, not a list). Unset → those routes return 403 and startup logs a warning |
| `APEX_READ_KEY` | Optional second bearer for GET report endpoints only (cron/report scripts). Never unlocks writes or `/dashboard`. Empty → off |
| `DOTTIE_EXCLUDED_INSTALL_IDS` | Comma-separated install ids dropped at ingest (dev machines). Empty → none |
| `DOTTIE_EXCLUDED_EMAILS` | Comma-separated emails dropped at ingest. Case-insensitive. Empty → none |
| `REPORT_EXCLUDE_EMAILS` | Comma-separated emails hidden from daily-report person lists only (totals/DAU unaffected). Trimmed, case-insensitive. Empty → exclude nothing |
| `REPORT_EXCLUDE_INSTALLS` | Comma-separated install ids hidden from daily-report person lists only. Trimmed, case-insensitive. Empty → exclude nothing |
| `FEEDBACK_DAILY_BUDGET` | Per-site feedback submissions per rolling 24h (default `1000`) |
| `FEEDBACK_LEGACY_KEYS` | Optional `pk_a=<site id>,pk_b=<site id>` map so embeds from the older feedback-assistant widget (`data-project="pk_…"`) keep working. Unset → off |
| `REGISTER_IP_HOURLY_MAX` | New accounts one IP may create per hour (default `10`). Counted in the DB, so it survives a restart; re-registering a known email never counts |
| `PRO_JWT_SECRET` | Optional HS256 secret for verifying/minting JWT bearers. Empty → opaque 64-hex tokens. Never logged |
| `PRO_STATS_URL` | Optional upstream for the daily report's Pro fleet/refund block (Dottie Pro integration). Unset → no fetch, sections print `unavailable`. The request carries `APEX_ADMIN_KEY`, so the host must be loopback or `PRO_STATS_ALLOWED_HOST`; anything else is skipped with a startup warning |
| `PRO_STATS_ALLOWED_HOST` | The one non-loopback host `PRO_STATS_URL` may use (exact hostname, e.g. `pro.internal`). Empty → loopback only |
| `DOTTIE_LATEST_VERSION` / `DOTTIE_MIN_VERSION` / `DOTTIE_DOWNLOAD_URL` | Values returned by `GET /api/version` |

Generate fresh keys with:

```bash
openssl rand -hex 32
```

<br />

## Self-hosting

Nothing here depends on Dottie's infrastructure. You need Rust (stable) and, only if you change the dashboard, Bun or Node.

1. Generate the admin key

   ```bash
   openssl rand -hex 32   # APEX_ADMIN_KEY (admin reads + /dashboard)
   ```

2. Run from source

   ```bash
   cd backend-rust
   export APEX_ADMIN_KEY=...
   export DB_PATH=./databases/analytics.db   # created on first run; migrations run at startup
   cargo run --release
   ```

3. Or run the container

   ```bash
   docker build -f Dockerfile.rust -t apex-analytics .
   docker run -p 8080:8080 \
     -e APEX_ADMIN_KEY=... \
     -e DB_PATH=/data/analytics.db -v analytics-data:/data \
     apex-analytics
   ```

4. Open `http://localhost:8080/dashboard` and paste `APEX_ADMIN_KEY` once
5. Create a site (`POST /api/sites`, admin bearer) and embed its `write_key`:

   ```html
   <script defer src="https://your-host/script.js" data-website-id="<write_key>"></script>
   ```

6. Optional
   - `APEX_READ_KEY` for cron/report scripts
   - `REPORT_EXCLUDE_EMAILS` / `REPORT_EXCLUDE_INSTALLS` to hide your own accounts from daily-report person lists
   - `GITHUB_TOKEN` + `GITHUB_REPOS` (+ `GITHUB_SITE_LINKS`) for the growth tracker
   - `LANDING_ALLOWED_ORIGIN` if you use the landing-page mirror
   - Add the feedback widget with the same `write_key`: `<script defer src="https://your-host/feedback.js" data-website-id="<write_key>"></script>`

Put it behind a TLS proxy and mount a persistent volume for `DB_PATH`. Back up the SQLite file.

<br />

## Tech Stack

| Technology | Purpose |
|---|---|
| **Rust** | Language / runtime |
| **axum + tokio** | Async HTTP framework |
| **sqlx** | Compile-time-checked SQL, bundled SQLite (JSON1) |
| **SQLite** | Embedded storage (WAL mode) |

<br />

## Architecture

Single axum app: `src/app.rs` (router + middleware + per-route rate limits + admin gating), `src/routes.rs` (handlers), `src/reports.rs` (report SQL), `src/db.rs` + `migrations/` (sqlx access and schema). The admin dashboard is a React/Vite/Tailwind SPA (`dashboard/`, built with `@stevederico/skateboard-ui`) built to `dashboard/dist` and embedded into the binary via `rust-embed`, served at `/dashboard` — still one distroless binary, no static dir shipped.

Every request passes through `noindex` middleware. Writes pass through rate limiting + the desktop-key guard. Admin reads pass through rate limiting + the admin-key guard. Failed guards return `404` (not `403`) so unauthorized clients see no evidence the endpoint exists.

<br />

## Deployment

Any container host works. `Dockerfile.rust` builds a distroless image (no shell); on Railway set `RAILWAY_DOCKERFILE_PATH=Dockerfile.rust`. Mount a persistent volume and set `DB_PATH` to a path on it to survive restarts.

### Docker (local)

```bash
docker build -f Dockerfile.rust -t apex-analytics .
docker run -p 8080:8080 \
  -e APEX_ADMIN_KEY=... \
  -e DB_PATH=/data/apex-api.db \
  -v apex-data:/data \
  apex-analytics
```

### curl sanity check

```bash
# should be 404 (GET on a POST-only route)
curl -sI https://your-host/api/register

# should be 200 — register is an unauthenticated bootstrap; returns a bearer token
curl -X POST https://your-host/api/register \
  -H "Content-Type: application/json" \
  -d '{"name":"Test","email":"t@t.com","install_id":"abc","version":"7.99","os_version":"14.0"}'

# should be {users, dau, mau, pings_24h}
curl https://your-host/api/stats \
  -H "Authorization: Bearer <APEX_ADMIN_KEY>"
```

<br />

## License

MIT License — see [LICENSE](LICENSE).
