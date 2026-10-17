#!/usr/bin/env node
// Print a daily analytics snapshot. Hits /api/stats, /api/stats/retention,
// /api/stats/active, /api/stats/errors and pretty-prints. Read-only, so it
// prefers the low-privilege DOTTIE_READ_KEY and falls back to DOTTIE_ADMIN_KEY.
//
// Usage:
//   DOTTIE_READ_KEY=... DOTTIE_API_BASE=https://analytics.example.com node scripts/daily_report.js

const BASE = process.env.DOTTIE_API_BASE;
if (!BASE) {
  console.error("DOTTIE_API_BASE not set (e.g. https://analytics.example.com)");
  process.exit(1);
}
const KEY = process.env.DOTTIE_READ_KEY || process.env.DOTTIE_ADMIN_KEY;
if (!KEY) {
  console.error("DOTTIE_READ_KEY (or DOTTIE_ADMIN_KEY) not set");
  process.exit(1);
}

const headers = {
  authorization: `Bearer ${KEY}`,
  // Cloudflare 403s empty UAs.
  "user-agent": "dottie-daily-report/1.0",
};

async function get(path) {
  const res = await fetch(`${BASE}${path}`, { headers });
  if (!res.ok) throw new Error(`GET ${path} → HTTP ${res.status}`);
  return res.json();
}

function pct(n) {
  return n == null ? "—" : `${Math.round(n * 100)}%`;
}

function bar(value, max, width = 20) {
  if (!max) return "";
  const filled = Math.round((value / max) * width);
  return "█".repeat(filled) + "·".repeat(width - filled);
}

const [stats, retention, active, errors] = await Promise.all([
  get("/api/stats"),
  get("/api/stats/retention?weeks=8"),
  get("/api/stats/active?days=14"),
  get("/api/stats/errors?hours=24"),
]);

const today = new Date().toISOString().slice(0, 10);

console.log(`=== Dottie metrics (${today}) ===\n`);

console.log(`Users registered:       ${stats.users}`);
console.log(`DAU / WAU / MAU:        ${stats.dau} / ${stats.wau} / ${stats.mau}  (launches)`);
console.log(
  `DAU / WAU / MAU:        ${stats.dau_engaged} / ${stats.wau_engaged} / ${stats.mau_engaged}  (engaged — chatted or voiced)`
);
console.log(
  `Stickiness (DAU/MAU):   ${stats.stickiness == null ? "—" : pct(stats.stickiness)}  ${
    stats.stickiness >= 0.2 ? "✓ healthy" : stats.stickiness == null ? "" : "below 20% target"
  }`
);
console.log(`Pings / events (24h):   ${stats.pings_24h} / ${stats.events_24h}\n`);

console.log("Daily active (last 14 days):");
const maxDau = Math.max(...active.days.map((d) => d.dau), 1);
for (const d of active.days) {
  console.log(
    `  ${d.date}  dau ${String(d.dau).padStart(3)}  engaged ${String(d.dau_engaged).padStart(3)}  ${bar(d.dau, maxDau)}`
  );
}
console.log();

console.log("Cohort retention (weekly):");
console.log("  Week starting    size   engaged   D1        D7        D30");
for (const c of retention.cohorts) {
  const fmt = (b) =>
    b.of === 0 ? "   —    " : `${pct(b.rate).padEnd(4)} (${b.returned}/${b.of})`.padEnd(10);
  console.log(
    `  ${c.week}      ${String(c.size).padStart(3)}     ${String(c.engaged).padStart(3)}     ${fmt(c.d1)}${fmt(c.d7)}${fmt(c.d30)}`
  );
}
console.log();

console.log(`Top events (24h):`);
for (const e of stats.top_events_24h.slice(0, 8)) {
  console.log(`  ${String(e.count).padStart(5)}  ${e.name}`);
}
console.log();

console.log(`Top error sites (24h, ${errors.total} total):`);
for (const e of errors.top.slice(0, 8)) {
  const loc = `${e.file}:${e.function}${e.line ? `:${e.line}` : ""}`;
  console.log(`  ${String(e.count).padStart(5)}  ${loc}`);
}
