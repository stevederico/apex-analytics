#!/usr/bin/env bash
# Pull history from live growth-chart and push into dottie-analytics.
# Usage:
#   DOTTIE_ADMIN_KEY=… ./scripts/import_growth_chart.sh <dottie_base> <growth_base>
set -euo pipefail

DOTTIE="${1:?usage: import_growth_chart.sh <dottie_base> <growth_base>}"
GROWTH="${2:?usage: import_growth_chart.sh <dottie_base> <growth_base>}"
UA="${DOTTIE_UA:-Mozilla/5.0 Dottie/2026.7.12}"
KEY="${DOTTIE_ADMIN_KEY:?set DOTTIE_ADMIN_KEY}"
TMP=$(mktemp -d)
trap 'rm -rf "$TMP"' EXIT

echo "== fetch downloads from $GROWTH =="
curl -sS -A "$UA" "$GROWTH/api/downloads" -o "$TMP/downloads.json"
python3 - <<PY
import json
from pathlib import Path
p = Path("$TMP/downloads.json")
d = json.loads(p.read_text())
print(f"  downloads rows: {len(d)}")
PY

echo "== fetch metrics =="
: > "$TMP/metrics.jsonl"
for m in stars forks clones views; do
  curl -sS -A "$UA" "$GROWTH/api/metrics?metric=$m" -o "$TMP/m_$m.json"
  python3 - <<PY
import json
from pathlib import Path
rows = json.loads(Path("$TMP/m_$m.json").read_text())
print(f"  $m: {len(rows)}")
with open("$TMP/metrics.jsonl", "a") as f:
    for r in rows:
        f.write(json.dumps(r) + "\n")
PY
done
python3 - <<'PY'
import json
from pathlib import Path
import os
tmp = os.environ.get("TMP") or "/tmp"
# rebuild from files written above — use fixed path via env
PY

# Build one metrics array
python3 - <<PY
import json
from pathlib import Path
tmp = Path("$TMP")
metrics = []
for m in ("stars", "forks", "clones", "views"):
    metrics.extend(json.loads((tmp / f"m_{m}.json").read_text()))
downloads = json.loads((tmp / "downloads.json").read_text())
print(f"  total metrics: {len(metrics)}")

# Chunk to stay under 10k rows / request
CHUNK = 4000

def chunks(arr, n):
    for i in range(0, len(arr), n):
        yield arr[i:i+n]

import urllib.request

def post(body):
    req = urllib.request.Request(
        "$DOTTIE/api/import/github-growth",
        data=json.dumps(body).encode(),
        headers={
            "Authorization": "Bearer $KEY",
            "Content-Type": "application/json",
            "User-Agent": "$UA",
        },
        method="POST",
    )
    with urllib.request.urlopen(req, timeout=120) as resp:
        return json.loads(resp.read().decode())

total_dl = total_mt = 0
# downloads first in chunks
for i, part in enumerate(chunks(downloads, CHUNK)):
    r = post({"downloads": part, "metrics": []})
    print(f"  dl chunk {i}: {r}")
    total_dl += r.get("downloads_imported", 0)

for i, part in enumerate(chunks(metrics, CHUNK)):
    r = post({"downloads": [], "metrics": part})
    print(f"  mt chunk {i}: {r}")
    total_mt += r.get("metrics_imported", 0)

print(f"== done downloads={total_dl} metrics={total_mt} ==")
PY
