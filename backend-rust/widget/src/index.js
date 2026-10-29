import stylesText from './styles.css?inline';

// Apex feedback widget, ported from feedback-assistant. One tag:
//   <script defer src="https://your-host/feedback.js" data-website-id="WRITE_KEY">
// data-website-id is the site's write_key (the same one /script.js takes).
// Submissions go to <script origin>/api/feedback as multipart/form-data: a
// CORS "simple" request, so there is no preflight and no custom header.

// Origin that served this widget script: the API host and where the lazily
// loaded html2canvas lives. Captured at module load while
// document.currentScript is still valid.
const WIDGET_ORIGIN = (() => {
  try {
    const s = typeof document !== 'undefined' ? document.currentScript : null;
    return s && s.src ? new URL(s.src, document.baseURI).origin : '';
  } catch {
    return '';
  }
})();

const SCREENSHOT_MAX_WIDTH = 1600;
const MAX_MESSAGE_CHARS = 5000; // server: UTF-16 units, same as String.length

// Server error codes → what the user sees.
const ERRORS = {
  message_required: 'Add a message before sending',
  message_too_long: `Message must be ${MAX_MESSAGE_CHARS} characters or fewer`,
  screenshot_too_large: 'Screenshot is too large',
  screenshot_type: 'Screenshot must be JPEG or PNG',
  daily_budget_exceeded: 'Too much feedback today, try again later',
};

const STATE = {
  initialized: false,
  config: {
    website: null,
    apiUrl: WIDGET_ORIGIN,
    appVersion: null,
  },
  user: null,
  refs: {},
  ui: {
    message: '',
    screenshotBlob: null,    // JPEG blob, sent with the message
    screenshotPreview: null, // data URL for the in-widget thumbnail
    capturing: false,
    submitting: false,
  },
};

function setText(el, text) {
  el.textContent = text == null ? '' : String(text);
}

/**
 * Tiny hyperscript helper: builds real DOM nodes (never innerHTML), so the
 * widget works under `require-trusted-types-for 'script'`. `text` sets
 * textContent; `disabled` sets the property; everything else is an attribute.
 */
function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === 'class') node.className = v;
    else if (k === 'text') node.textContent = v;
    else if (k === 'disabled') { if (v) node.disabled = true; }
    else node.setAttribute(k, v);
  }
  for (const child of children) {
    if (child == null) continue;
    node.appendChild(typeof child === 'string' ? document.createTextNode(child) : child);
  }
  return node;
}

/**
 * Attach widget CSS to the shadow root. Prefers a Constructable Stylesheet
 * (not subject to the host page's `style-src` CSP); falls back to a <style>
 * element on older browsers.
 */
function applyStyles(shadow) {
  try {
    if (shadow.adoptedStyleSheets !== undefined && typeof CSSStyleSheet === 'function') {
      const sheet = new CSSStyleSheet();
      sheet.replaceSync(stylesText);
      shadow.adoptedStyleSheets = [sheet];
      return;
    }
  } catch {
    /* fall through to <style> */
  }
  shadow.appendChild(el('style', { text: stylesText }));
}

function svgIcon(build) {
  const ns = 'http://www.w3.org/2000/svg';
  const svg = document.createElementNS(ns, 'svg');
  for (const [k, v] of Object.entries({
    viewBox: '0 0 24 24', width: '16', height: '16', fill: 'none', stroke: 'currentColor',
    'stroke-width': '2', 'stroke-linecap': 'round', 'stroke-linejoin': 'round', 'aria-hidden': 'true',
  })) svg.setAttribute(k, v);
  const add = (tag, attrs) => {
    const n = document.createElementNS(ns, tag);
    for (const [k, v] of Object.entries(attrs)) n.setAttribute(k, v);
    svg.appendChild(n);
  };
  build(add);
  return svg;
}

/** Lucide-style camera icon for the screenshot button. */
const iconCamera = () => svgIcon((add) => {
  add('path', { d: 'M14.5 4h-5L7 7H4a2 2 0 0 0-2 2v9a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2V9a2 2 0 0 0-2-2h-3l-2.5-3z' });
  add('circle', { cx: '12', cy: '13', r: '3' });
});

/** Lucide-style monitor icon for share-screen. */
const iconMonitor = () => svgIcon((add) => {
  add('rect', { width: '20', height: '14', x: '2', y: '3', rx: '2' });
  add('path', { d: 'M8 21h8' });
  add('path', { d: 'M12 17v4' });
});

function buildPopover() {
  const shotBtn = el('button', {
    class: 'fa-btn fa-btn-icon fa-screenshot-btn', type: 'button', 'aria-label': 'Attach screenshot',
    title: 'Screenshot',
  }, iconCamera());
  const shareBtn = el('button', {
    class: 'fa-btn fa-btn-icon fa-sharescreen-btn', type: 'button', 'aria-label': 'Share screen',
    title: 'Share screen (asks permission)',
  }, iconMonitor());
  const header = el('div', { class: 'fa-header' },
    el('div', { class: 'fa-title', text: 'Feedback' }),
    el('div', { class: 'fa-header-tools' }, shotBtn, shareBtn),
  );

  const textarea = el('textarea', {
    class: 'fa-textarea', rows: '5', maxlength: String(MAX_MESSAGE_CHARS),
    placeholder: "What's on your mind? Bugs, ideas, anything…",
  });

  const panel = el('div', { class: 'fa-panel' },
    textarea,
    el('div', { class: 'fa-screenshot-slot' }),
    el('div', { class: 'fa-actions' },
      el('button', { class: 'fa-btn fa-btn-primary fa-send-btn', type: 'button', disabled: true }, 'Send'),
    ),
  );

  return { header, panel, textarea };
}

function ensureShadowRoot() {
  if (STATE.refs.host) return STATE.refs;

  const host = document.createElement('div');
  host.setAttribute('data-apex-feedback', '');
  host.setAttribute('data-dottie-feedback', ''); // pre-rename hook for host-page CSS
  host.style.all = 'initial';
  document.body.appendChild(host);

  const shadow = host.attachShadow({ mode: 'open' });
  applyStyles(shadow);

  const root = el('div', { class: 'fa-root' });
  shadow.appendChild(root);

  const orb = el('button', { type: 'button', class: 'fa-orb', 'aria-label': 'Open feedback' });
  orb.addEventListener('click', () => togglePopover());
  root.appendChild(orb);

  const popover = el('div', { class: 'fa-popover', role: 'dialog', 'aria-label': 'Feedback' });
  popover.dataset.open = 'false';
  const built = buildPopover();
  popover.appendChild(built.header);
  popover.appendChild(built.panel);
  root.appendChild(popover);

  const toast = el('div', { class: 'fa-toast', role: 'status' });
  toast.dataset.visible = 'false';
  root.appendChild(toast);

  const textarea = built.textarea;
  textarea.addEventListener('input', (e) => {
    STATE.ui.message = e.target.value;
    popover.querySelector('.fa-send-btn').disabled =
      !STATE.ui.message.trim() || STATE.ui.submitting;
  });

  popover.querySelector('.fa-screenshot-btn').addEventListener('click', () => handleCapture());
  popover.querySelector('.fa-sharescreen-btn').addEventListener('click', () => handleShareScreen());
  popover.querySelector('.fa-send-btn').addEventListener('click', () => handleSend());

  document.addEventListener('click', (e) => {
    if (popover.dataset.open !== 'true') return;
    const path = typeof e.composedPath === 'function' ? e.composedPath() : [];
    if (path.includes(host) || host.contains(e.target)) return;
    setOpen(false);
  });

  STATE.refs = { host, shadow, root, orb, popover, toast, textarea };
  return STATE.refs;
}

function resetScreenshot() {
  STATE.ui.screenshotBlob = null;
  STATE.ui.screenshotPreview = null;
}

function renderScreenshot() {
  const { popover } = STATE.refs;
  const slot = popover.querySelector('.fa-screenshot-slot');
  const captureBtns = popover.querySelectorAll('.fa-screenshot-btn, .fa-sharescreen-btn');
  slot.textContent = '';
  if (!STATE.ui.screenshotPreview) {
    captureBtns.forEach((b) => { b.style.display = ''; });
    return;
  }
  captureBtns.forEach((b) => { b.style.display = 'none'; });
  const wrap = el('div', { class: 'fa-screenshot' });
  const img = el('img', { alt: 'Screenshot preview' });
  img.src = STATE.ui.screenshotPreview;
  const remove = el('button', { type: 'button', 'aria-label': 'Remove screenshot' }, '×');
  remove.addEventListener('click', () => { resetScreenshot(); renderScreenshot(); });
  wrap.appendChild(img);
  wrap.appendChild(remove);
  slot.appendChild(wrap);
}

function setOpen(open) {
  const { popover } = STATE.refs;
  popover.dataset.open = open ? 'true' : 'false';
  if (open) {
    renderScreenshot();
    setTimeout(() => STATE.refs.textarea.focus(), 0);
  }
}

function togglePopover() {
  setOpen(STATE.refs.popover.dataset.open !== 'true');
}

function showToast(message, tone = 'info') {
  const { toast } = STATE.refs;
  setText(toast, message);
  toast.dataset.tone = tone;
  toast.dataset.visible = 'true';
  clearTimeout(showToast._t);
  showToast._t = setTimeout(() => {
    toast.dataset.visible = 'false';
  }, 2400);
}

/** Lazily load self-hosted html2canvas-pro as a classic script; resolves the global. */
function loadHtml2canvas() {
  if (typeof window !== 'undefined' && window.html2canvas) return Promise.resolve(window.html2canvas);
  if (loadHtml2canvas._p) return loadHtml2canvas._p;
  loadHtml2canvas._p = new Promise((resolve, reject) => {
    const s = document.createElement('script');
    s.src = `${STATE.config.apiUrl}/feedback/html2canvas-v2.js`;
    s.async = true;
    s.onload = () => (window.html2canvas ? resolve(window.html2canvas) : reject(new Error('html2canvas load failed')));
    s.onerror = () => { loadHtml2canvas._p = null; reject(new Error('html2canvas load failed')); };
    document.head.appendChild(s);
  });
  return loadHtml2canvas._p;
}

/** Default capture: render the current viewport to a canvas. No permission prompt. */
async function captureWithHtml2canvas() {
  const html2canvas = await loadHtml2canvas();
  const scale = Math.min(1, SCREENSHOT_MAX_WIDTH / Math.max(window.innerWidth, 1));
  return html2canvas(document.body, {
    useCORS: true,
    allowTaint: false,
    logging: false,
    backgroundColor: '#ffffff',
    x: window.scrollX,
    y: window.scrollY,
    width: window.innerWidth,
    height: window.innerHeight,
    windowWidth: document.documentElement.scrollWidth,
    windowHeight: document.documentElement.scrollHeight,
    scale,
    ignoreElements: (node) => node === STATE.refs.host,
  });
}

/** Secondary capture: getDisplayMedia (asks permission, captures real pixels). */
async function captureScreenshot() {
  const stream = await navigator.mediaDevices.getDisplayMedia({
    video: { displaySurface: 'browser' },
    audio: false,
  });
  const track = stream.getVideoTracks()[0];
  const video = document.createElement('video');
  video.srcObject = stream;
  video.muted = true;
  await video.play();
  await new Promise((r) => setTimeout(r, 250));
  const scale = Math.min(1, SCREENSHOT_MAX_WIDTH / Math.max(video.videoWidth, 1));
  const canvas = document.createElement('canvas');
  canvas.width = Math.round(video.videoWidth * scale);
  canvas.height = Math.round(video.videoHeight * scale);
  canvas.getContext('2d').drawImage(video, 0, 0, canvas.width, canvas.height);
  track.stop();
  video.srcObject = null;
  return canvas;
}

function canvasToBlob(canvas, type, quality) {
  return new Promise((resolve, reject) => {
    canvas.toBlob((b) => (b ? resolve(b) : reject(new Error('encode failed'))), type, quality);
  });
}

/** Keep a preview thumbnail + the JPEG that goes out with the message. */
async function finishCapture(canvas) {
  STATE.ui.screenshotPreview = canvas.toDataURL('image/jpeg', 0.5);
  renderScreenshot();
  STATE.ui.screenshotBlob = await canvasToBlob(canvas, 'image/jpeg', 0.7);
}

function setCaptureBusy(busy) {
  const { popover } = STATE.refs;
  popover.querySelectorAll('.fa-screenshot-btn, .fa-sharescreen-btn').forEach((b) => { b.disabled = busy; });
}

async function handleCapture() {
  if (STATE.ui.capturing) return;
  STATE.ui.capturing = true;
  setCaptureBusy(true);
  try {
    const canvas = await captureWithHtml2canvas();
    await finishCapture(canvas);
  } catch (err) {
    resetScreenshot();
    renderScreenshot();
    const msg = /load failed/.test(err?.message || '') ? 'Screenshot unavailable' : 'Could not capture screenshot';
    showToast(msg, 'error');
  } finally {
    STATE.ui.capturing = false;
    setCaptureBusy(false);
  }
}

async function handleShareScreen() {
  if (STATE.ui.capturing) return;
  setOpen(false);
  STATE.ui.capturing = true;
  try {
    const canvas = await captureScreenshot();
    setOpen(true);
    await finishCapture(canvas);
  } catch (err) {
    setOpen(true);
    resetScreenshot();
    renderScreenshot();
    if (err?.name !== 'NotAllowedError' && err?.name !== 'AbortError') {
      showToast('Could not capture screenshot', 'error');
    }
  } finally {
    STATE.ui.capturing = false;
  }
}

async function handleSend() {
  const trimmed = STATE.ui.message.trim();
  if (!trimmed) {
    showToast(ERRORS.message_required, 'error');
    return;
  }
  if (!STATE.config.website) {
    showToast('Widget not initialized', 'error');
    return;
  }

  STATE.ui.submitting = true;
  const sendBtn = STATE.refs.popover.querySelector('.fa-send-btn');
  setText(sendBtn, 'Sending…');
  sendBtn.disabled = true;

  try {
    const form = new FormData();
    const fields = {
      website: STATE.config.website,
      message: trimmed,
      url: window.location.href,
      appVersion: STATE.config.appVersion,
      endUserId: STATE.user?.id,
      endUserName: STATE.user?.name,
      endUserEmail: STATE.user?.email,
    };
    for (const [k, v] of Object.entries(fields)) {
      if (v != null && v !== '') form.append(k, String(v));
    }
    if (STATE.ui.screenshotBlob) form.append('file', STATE.ui.screenshotBlob, 'screenshot.jpg');
    const res = await fetch(`${STATE.config.apiUrl}/api/feedback`, {
      method: 'POST',
      body: form,
      credentials: 'omit',
    });
    if (!res.ok) {
      const body = await res.json().catch(() => ({}));
      throw new Error(ERRORS[body?.error] || `Send failed (${res.status})`);
    }
    showToast('Thanks, feedback sent');
    STATE.ui.message = '';
    resetScreenshot();
    STATE.refs.textarea.value = '';
    renderScreenshot();
    setOpen(false);
  } catch (err) {
    showToast(err?.message || 'Could not send feedback', 'error');
  } finally {
    STATE.ui.submitting = false;
    setText(sendBtn, 'Send');
    sendBtn.disabled = !STATE.ui.message.trim();
  }
}

// NOTE: default export (not named). vite's IIFE build exposes the module's
// default export as the global; a named export would expose the namespace
// object instead, hiding init/identify/show one level deeper.
const ApexFeedback = {
  init(options = {}) {
    if (!options.website) {
      console.warn('[ApexFeedback] website (the site write_key) is required');
      return;
    }
    STATE.config.website = options.website;
    STATE.config.apiUrl = (options.apiUrl || STATE.config.apiUrl || '').replace(/\/$/, '');
    STATE.config.appVersion = options.appVersion ?? null;
    STATE.initialized = true;
    ensureShadowRoot();
  },
  identify(user = {}) {
    STATE.user = {
      id: user.id ?? null,
      name: user.name ?? null,
      email: user.email ?? null,
    };
  },
  show() {
    if (!STATE.initialized) return;
    ensureShadowRoot();
    setOpen(true);
  },
};

// Auto-init from the tag. `defer` scripts run before DOMContentLoaded, when
// document.body exists, so the orb can mount straight away.
if (typeof document !== 'undefined') {
  const tag = document.currentScript;
  // data-project is the retired feedback-assistant tag's key (`pk_…`); the
  // server maps it to a site, so an old embed served this file keeps working.
  const website = tag && (tag.getAttribute('data-website-id') || tag.getAttribute('data-project'));
  if (website) {
    const start = () => ApexFeedback.init({
      website,
      apiUrl: tag.getAttribute('data-host') || WIDGET_ORIGIN,
      appVersion: tag.getAttribute('data-app-version'),
    });
    if (document.body) start();
    else document.addEventListener('DOMContentLoaded', start, { once: true });
  }
}

if (typeof window !== 'undefined') {
  // DottieFeedback is the pre-rename global; pages that call it keep working.
  window.ApexFeedback = window.DottieFeedback = ApexFeedback;
}

export default ApexFeedback;
