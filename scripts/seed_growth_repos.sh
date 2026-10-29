#!/usr/bin/env bash
# Seed GitHub growth repos + snapshot downloads/metrics on a running apex-analytics.
# Usage:
#   APEX_ADMIN_KEY=… ./scripts/seed_growth_repos.sh <base_url> <owner/name>...
# (the pre-rename DOTTIE_ADMIN_KEY also works)
set -euo pipefail

BASE="${1:?usage: seed_growth_repos.sh <base_url> <owner/name>...}"
shift
[ "$#" -gt 0 ] || { echo "usage: seed_growth_repos.sh <base_url> <owner/name>..." >&2; exit 1; }
UA="${DOTTIE_UA:-Mozilla/5.0 Dottie/2026.7.12}"
KEY="${APEX_ADMIN_KEY:-${DOTTIE_ADMIN_KEY:?set APEX_ADMIN_KEY}}"

REPOS=("$@")

auth=(-A "$UA" -H "Authorization: Bearer $KEY" -H "Content-Type: application/json")

echo "== list repos =="
curl -sS "${auth[@]}" "$BASE/api/downloads/repos" | python3 -m json.tool

for r in "${REPOS[@]}"; do
  echo "== add $r =="
  code=$(curl -sS -o /tmp/add_repo.json -w "%{http_code}" "${auth[@]}" \
    -X POST "$BASE/api/repos" -d "{\"repo\":\"$r\"}")
  echo "  HTTP $code $(head -c 120 /tmp/add_repo.json)"
done

echo "== snapshot all downloads =="
curl -sS "${auth[@]}" -X POST "$BASE/api/downloads/snapshot" -d '{}' | python3 -m json.tool

echo "== snapshot all metrics =="
curl -sS "${auth[@]}" -X POST "$BASE/api/metrics/snapshot" -d '{}' | python3 -m json.tool

echo "== final repos =="
curl -sS "${auth[@]}" "$BASE/api/downloads/repos" | python3 -m json.tool

echo "== sample latest ${REPOS[0]} =="
curl -sS "${auth[@]}" \
  "$BASE/api/downloads/latest?repo=${REPOS[0]//\//%2F}" | python3 -m json.tool
