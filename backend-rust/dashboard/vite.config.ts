import { defineConfig, type Plugin } from 'vite';
import react from '@vitejs/plugin-react-swc';
import tailwindcss from '@tailwindcss/vite';
import path from 'node:path';
import { readFileSync } from 'node:fs';

/** Fold the entry JS + CSS into index.html so the dashboard ships as one HTML
 *  document. Cloudflare (when it fronts the API) 403s the separate hashed
 *  /dashboard/assets/*.js on cache-miss (new hash every deploy), which blanks
 *  the SPA; the /dashboard HTML navigation itself is served fine, so inlining
 *  the JS/CSS sidesteps the block with no external script subresource. Fonts
 *  stay external — their hashes are content-stable, so they cache (HIT) and are
 *  never the blocked request. */
function inlineDashboard(): Plugin {
  return {
    name: 'inline-dashboard',
    enforce: 'post',
    generateBundle(_options, bundle) {
      const html = Object.values(bundle).find(
        (f) => f.type === 'asset' && f.fileName === 'index.html',
      );
      if (!html || html.type !== 'asset' || typeof html.source !== 'string') return;
      let out = html.source;
      for (const file of Object.values(bundle)) {
        if (file.type === 'chunk' && file.isEntry) {
          // Escape any `</script` in the code so it can't close the inline tag.
          const code = file.code.replace(/<\/script/gi, '<\\/script');
          // Function replacer: the code contains `$` sequences that a string
          // replacement would mis-interpret ($&, $1, …), corrupting the bundle.
          out = out.replace(
            new RegExp(`<script[^>]*src="[^"]*${file.fileName}"[^>]*></script>`),
            () => `<script type="module">${code}</script>`,
          );
          delete bundle[file.fileName];
        } else if (file.type === 'asset' && file.fileName.endsWith('.css') && typeof file.source === 'string') {
          const css = file.source;
          out = out.replace(
            new RegExp(`<link[^>]*href="[^"]*${file.fileName}"[^>]*>`),
            () => `<style>${css}</style>`,
          );
          delete bundle[file.fileName];
        }
      }
      html.source = out;
    },
  };
}

/** Latest `## X.Y.Z` from repo CHANGELOG — shown in Settings. */
function appVersion(): string {
  try {
    const md = readFileSync(path.resolve(process.cwd(), '../../CHANGELOG.md'), 'utf8');
    return md.match(/^## (\d+\.\d+\.\d+)/m)?.[1] ?? '0.0.0';
  } catch {
    return '0.0.0';
  }
}

// The dashboard is embedded into the Rust binary and served under /dashboard,
// so all emitted asset URLs must be absolute under that prefix.
export default defineConfig({
  base: '/dashboard/',
  plugins: [react(), tailwindcss(), inlineDashboard()],
  define: {
    __APEX_VERSION__: JSON.stringify(appVersion()),
  },
  resolve: {
    // skateboard-ui expects a single React instance.
    dedupe: ['react', 'react-dom'],
    alias: { '@': path.resolve(process.cwd(), './src') },
  },
  // Prebundle skateboard-ui + React so CJS/ESM interop is correct. Excluding
  // skateboard-ui made dev load raw package files and broke `react/jsx-runtime`
  // ("does not provide an export named 'jsx'").
  optimizeDeps: {
    include: [
      'react',
      'react-dom',
      'react/jsx-runtime',
      'react/jsx-dev-runtime',
      '@stevederico/skateboard-ui',
    ],
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    // One binary ships this; keep the bundle self-contained and cache-busted.
    assetsInlineLimit: 0,
  },
  server: {
    port: 5173,
    // Local dev: SPA on :5173, Rust API on :8080 — proxy REST + gRPC-Web paths.
    proxy: {
      '/api': 'http://127.0.0.1:8080',
      '/health': 'http://127.0.0.1:8080',
      '/apex.admin.v1.Admin': 'http://127.0.0.1:8080',
    },
  },
});
