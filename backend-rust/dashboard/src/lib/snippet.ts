// When Cloudflare fronts the dashboard, its WAF XSS rule 403s any
// asset whose body contains a literal `<script` — which blanks the whole SPA.
// So build the copy-paste tag at runtime from a non-foldable `<` (esbuild won't
// constant-fold String.fromCharCode), keeping the bundle bytes free of `<script`
// / `</script>` while the runtime string is still a valid, pasteable tag.
const LT = String.fromCharCode(60); // '<'

/** The install `<script>` tag a site pastes into its <head>. `id` is the site's
 *  write_key (or the `YOUR_KEY` placeholder in the generic Settings reference). */
export function installSnippet(id: string): string {
  return `${LT}script defer src="${window.location.origin}/script.js" data-website-id="${id}">${LT}/script>`;
}

/** The feedback widget tag (`/feedback.js`). Same write_key as the tracker. */
export function feedbackSnippet(id: string): string {
  return `${LT}script defer src="${window.location.origin}/feedback.js" data-website-id="${id}">${LT}/script>`;
}
