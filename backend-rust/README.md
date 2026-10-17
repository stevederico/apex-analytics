# dottie-analytics — Rust (axum + sqlx)

The dottie-analytics service, on the mainstream Rust web stack: **axum + tokio +
sqlx (bundled SQLite, JSON1)**. This is the only deployed artifact. It began as a
byte-for-byte port of the original Node service and has since moved ahead of it:
multi-tenant web analytics, feedback, the landing mirror, and the admin dashboard
exist only here.

## Build & test

```sh
cargo build --release      # binary → target/release/dottie-analytics
cargo test                 # integration + unit tests
```

## Run the server

The binary reads these env vars:

| Env | Default | Notes |
|---|---|---|
| `PORT` | `8080` | listen port |
| `DB_PATH` | `./databases/dottie-api.db` | SQLite file (also accepts `DATABASE_URL=sqlite://...`) |
| `DOTTIE_ADMIN_KEY` | `""` | admin report bearer key (empty disables) |
| `DOTTIE_LATEST_VERSION` | `7.99` | surfaced by `/api/version` |
| `DOTTIE_MIN_VERSION` | `7.99` | surfaced by `/api/version` |
| `DOTTIE_DOWNLOAD_URL` | GitHub releases zip | surfaced by `/api/version` |

The full list (read key, GitHub growth, landing origin, report exclusions, feedback budget) is in the [root README](../README.md#environment-variables).

```sh
# Boot against a throwaway copy of an existing DB on port 8081:
cp databases/dottie-api.db /tmp/rust.db
DB_PATH=/tmp/rust.db PORT=8081 \
DOTTIE_ADMIN_KEY=devadmin \
  ./backend-rust/target/release/dottie-analytics
```

`SIGTERM` (container stop / redeploy) and `Ctrl-C` trigger a graceful shutdown: in-flight
requests drain before the process exits.

## Point the ops tooling at the server

Both ops scripts (`scripts/daily_report.js`, `scripts/smoke.sh`) are plain HTTP
clients and require `DOTTIE_API_BASE`, so they run against any instance unchanged
(`daily_report.js` stays Node so it doesn't drag a TLS client crate like `reqwest`
into the binary for zero deploy benefit).

```sh
# Daily report:
DOTTIE_ADMIN_KEY=devadmin DOTTIE_API_BASE=http://127.0.0.1:8081 \
  node scripts/daily_report.js

# Smoke test (SMOKE_SITE_ID = a site write_key; the collector check posts to
# $DOTTIE_API_BASE/api/send unless SMOKE_UMAMI_URL overrides it):
DOTTIE_USER_TOKEN=<a user bearer> DOTTIE_ADMIN_KEY=devadmin SMOKE_SITE_ID=<write_key> \
DOTTIE_API_BASE=http://127.0.0.1:8081 ./scripts/smoke.sh
```

## Docker

`../Dockerfile.rust` — multi-stage (Rust builder → `distroless/cc-debian12`
runtime). SQLite is statically bundled by `libsqlite3-sys` (no external
`libsqlite3`; the builder runs `ldd` to prove it). Migrations are embedded into
the binary at compile time by `sqlx::migrate!`, so the runtime image omits the
`migrations/` dir.
