// Vendor html2canvas-pro (html2canvas fork that parses modern CSS colors such as
// oklch, which Tailwind v4 emits and html2canvas 1.x rejects) into widget/dist (embedded by rust-embed) as a self-hosted, immutably-cached asset.
// Runs after `vite build` (which empties dist), so the file survives. The widget
// lazy-loads it from /feedback/html2canvas-v2.js only when a screenshot is taken,
// keeping it out of the main IIFE bundle.
import { copyFileSync, mkdirSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const here = dirname(fileURLToPath(import.meta.url));

// Resolve the package's UMD build regardless of npm workspace hoisting.
const pkgMain = require.resolve('html2canvas-pro'); // .../html2canvas-pro/dist/html2canvas-pro.cjs
const src = resolve(dirname(pkgMain), 'html2canvas-pro.min.js');

const destDir = resolve(here, 'dist');
const dest = resolve(destDir, 'html2canvas-v2.js');

mkdirSync(destDir, { recursive: true });
copyFileSync(src, dest);
console.log('Vendored html2canvas-pro →', dest);
