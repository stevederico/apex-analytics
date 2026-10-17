# dottie-analytics

## Project Overview
Rust + Axum + SQLite (sqlx) service for app registration, install pings, stats, first-party web analytics (an Umami replacement), in-app feedback, and an admin dashboard. Built to be invisible to the public web: probes get 404s, bots are disallowed, no HTML at the public root.

## Architecture
- `backend-rust/` is the service and the only deployed artifact. axum + tokio + sqlx (bundled SQLite, JSON1). Built via `Dockerfile.rust` into one distroless binary.
  - `src/app.rs`: router, middleware, per-route rate limits, admin gating
  - `src/routes.rs`: request handlers
  - `src/reports.rs`: stats and report SQL (retention, activation, funnels, lifecycle, attribution, series, multi-tenant site reports)
  - `src/db.rs`: sqlx access and row models; `migrations/`: schema (sqlx migrations, never edit an applied one)
  - `src/feedback.rs`: feedback widget ingest; widget source in `widget/`
  - `src/feedback_legacy.rs`: compatibility routes for embeds of an older feedback widget (`FEEDBACK_LEGACY_KEYS` maps old keys to sites)
  - `src/github.rs`: GitHub release downloads and repo traffic collectors
  - `dashboard/`: admin SPA (React + Vite + Tailwind v4). Built to `dashboard/dist` and embedded into the binary via `rust-embed`, served at `/dashboard`. `dist/` is committed so `cargo build` and `cargo test` work without node
  - `tests/`: integration tests
- `scripts/`: ops HTTP clients for a running instance (`daily_report.js`, `smoke.sh`, `seed_growth_repos.sh`, `import_growth_chart.sh`). Each requires the base URL; none has a default host
- `docs/API.md`: endpoint reference

## Build / Run
```bash
# Dashboard SPA (only when its source changes; dist/ is committed):
cd backend-rust/dashboard && npm install && npm run build

# Feedback widget (only when its source changes; dist/ is committed):
cd backend-rust/widget && npm install && npm run build

cd backend-rust
export DOTTIE_ADMIN_KEY=$(openssl rand -hex 32)
cargo run     # listens on :8080
cargo test
```
See `README.md` for every environment variable.

## Key Patterns
- Per-user bearer for app writes (`desktop_bearer_guard`); admin bearer for stats and admin reads. There is no shared app key: any `X-Dottie-Client` request gets a 404
- Optional read-only report key (`DOTTIE_READ_KEY`): GET report endpoints only, never writes or the dashboard
- Per-user bearer from `/api/register` for anything that should be metered per user
- Timing-safe comparisons on every key check (`safe_equal`)
- Missing or wrong key returns 404, not 401/403, so endpoints look nonexistent to probes
- No blanket CORS. Public ingest (`/api/collect`, `/api/feedback`) reflects the site origin in-handler
- Per-IP rate limits on every `/api/*` route and the gRPC-Web admin service (static assets, `/health`, `/docs`, `/dashboard` are unlimited)
- `robots.txt` disallows all; `X-Robots-Tag: noindex, nofollow, noarchive` on every response; `/` is 404

## Conventions
- Add a migration for every schema change; keep migrations append-only
- Add an integration test in `tests/` for every new route
- Report SQL lives in `reports.rs`, not in handlers
- Rebuild and commit `dashboard/dist` or `widget/dist` when their source changes
- Operator-specific values (hosts, repos, site ids, origins) come from env, never from source

## Don't
- Don't return 401/403 on bad auth
- Don't add blanket CORS or serve HTML at `/`
- Don't weaken timing-safe comparisons or rate limits
- Don't log keys, tokens, or request bodies that carry them
