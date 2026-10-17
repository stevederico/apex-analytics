#!/usr/bin/env bash
# Live smoke test for a running dottie-analytics: hits the API and the
# Umami-compatible collector to prove events are actually being accepted.
#
# Sends a desktop-style User-Agent, since a Cloudflare bot rule in front of the
# API may 403 bare curl.
#
# Usage:
#   DOTTIE_API_BASE=... DOTTIE_USER_TOKEN=... DOTTIE_ADMIN_KEY=... SMOKE_SITE_ID=... ./scripts/smoke.sh
#
# DOTTIE_USER_TOKEN is a per-user /api/register bearer. Use one whose user
# already has an install_id: ingest fills an empty users.install_id, so a fresh
# user would get linked to the smoke install. The shared X-Dottie-Client key is
# retired; the script checks it now gets a 404.
#
# DOTTIE_API_BASE is the server to test, e.g. http://127.0.0.1:8080.
# SMOKE_SITE_ID is the site write_key the collector event is sent to.
# SMOKE_UMAMI_URL overrides the collector (default: $DOTTIE_API_BASE/api/send).
# The collector check writes one real `smoke.test` event into that site, so
# point SMOKE_SITE_ID at a dedicated smoke site, not a production one.
#
# Or rely on a .env at the repo root:
#   ./scripts/smoke.sh

set -euo pipefail

ENV_FILE="$(dirname "$0")/../.env"
if [ -f "$ENV_FILE" ]; then
  # shellcheck disable=SC1090
  set -a; source "$ENV_FILE"; set +a
fi

: "${DOTTIE_USER_TOKEN:?DOTTIE_USER_TOKEN not set}"
: "${DOTTIE_ADMIN_KEY:?DOTTIE_ADMIN_KEY not set}"
: "${DOTTIE_API_BASE:?DOTTIE_API_BASE not set (e.g. http://127.0.0.1:8080)}"
: "${SMOKE_SITE_ID:?SMOKE_SITE_ID not set (site write_key for the collector check)}"

UA="Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) Dottie/smoke-test"
API="$DOTTIE_API_BASE"
UMAMI="${SMOKE_UMAMI_URL:-$API/api/send}"
UMAMI_WEBSITE="$SMOKE_SITE_ID"
INSTALL_ID="smoke-test-$(date +%s)"

pass() { printf "  \033[32m✓\033[0m %s\n" "$1"; }
fail() { printf "  \033[31m✗\033[0m %s\n" "$1"; exit 1; }

echo "→ $API"

code=$(curl -s -o /dev/null -w "%{http_code}" -A "$UA" "$API/health")
[ "$code" = "200" ] && pass "GET /health → 200" || fail "GET /health → $code"

code=$(curl -s -o /dev/null -w "%{http_code}" -A "$UA" "$API/api/stats")
[ "$code" = "404" ] && pass "GET /api/stats (no key) → 404" || fail "GET /api/stats (no key) → $code (expected 404)"

code=$(curl -s -o /dev/null -w "%{http_code}" -A "$UA" \
  -H "Authorization: Bearer $DOTTIE_ADMIN_KEY" "$API/api/stats")
[ "$code" = "200" ] && pass "GET /api/stats (admin key) → 200" || fail "GET /api/stats (admin key) → $code"

code=$(curl -s -o /dev/null -w "%{http_code}" -A "$UA" \
  -H "Authorization: Bearer $DOTTIE_ADMIN_KEY" "$API/api/stats/errors?hours=1")
[ "$code" = "200" ] && pass "GET /api/stats/errors (admin key) → 200" || fail "GET /api/stats/errors → $code"

code=$(curl -s -o /dev/null -w "%{http_code}" -A "$UA" -X POST \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer $DOTTIE_USER_TOKEN" \
  -d "{\"install_id\":\"$INSTALL_ID\",\"name\":\"smoke.test\",\"version\":\"smoke\"}" \
  "$API/api/events")
[ "$code" = "200" ] && pass "POST /api/events (user bearer) → 200" || fail "POST /api/events → $code"

code=$(curl -s -o /dev/null -w "%{http_code}" -A "$UA" -X POST \
  -H "Content-Type: application/json" \
  -H "X-Dottie-Client: retired-shared-key" \
  -d "{\"install_id\":\"$INSTALL_ID\",\"name\":\"smoke.test\"}" \
  "$API/api/events")
[ "$code" = "404" ] && pass "POST /api/events (X-Dottie-Client) → 404" || fail "POST /api/events (X-Dottie-Client) → $code (expected 404)"

code=$(curl -s -o /dev/null -w "%{http_code}" -A "$UA" -X POST \
  -H "Content-Type: application/json" \
  -d "{\"install_id\":\"$INSTALL_ID\",\"name\":\"smoke.test\"}" \
  "$API/api/events")
[ "$code" = "404" ] && pass "POST /api/events (no key) → 404" || fail "POST /api/events (no key) → $code (expected 404)"

echo ""
echo "→ Umami collector ($UMAMI)"

code=$(curl -s -o /dev/null -w "%{http_code}" -A "$UA" -X POST \
  -H "Content-Type: application/json" \
  -d "{\"type\":\"event\",\"payload\":{\"website\":\"$UMAMI_WEBSITE\",\"url\":\"/app/smoke\",\"name\":\"smoke.test\",\"hostname\":\"smoke.example.com\",\"language\":\"en\",\"screen\":\"1920x1080\"}}" \
  "$UMAMI")
# Umami returns 200 (or 204) on success
if [ "$code" = "200" ] || [ "$code" = "204" ]; then
  pass "POST /api/send → $code"
else
  fail "POST /api/send → $code (expected 200/204)"
fi

echo ""
echo "All live checks passed — analytics pipeline is green."
