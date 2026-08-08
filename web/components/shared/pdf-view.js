import { html, nothing } from 'lit';
import { LightElement }  from '../../lib/base.js';
import { t }             from '../../lib/i18n.js';

/**
 * `<pdf-view src="blob:…">` — a continuous, scrollable PDF renderer built on the
 * vendored pdf.js.
 *
 * It exists because the obvious implementation — `<iframe src=<the pdf>>` — is
 * not portable. On iOS (Safari *and* every WKWebView, so the native shell too)
 * WebKit refuses to mount its PDF viewer inside a frame and paints a static
 * first-page thumbnail instead: no scroll, no other pages. That was the reported
 * bug. The desktop browsers do mount a viewer, but each mounts *its own* —
 * Chrome's toolbar, Safari's page-index sidebar, Firefox's own pdf.js — so the
 * same document looked different on every machine. Rendering the pages
 * ourselves answers both: one appearance everywhere, and pages that scroll.
 *
 * Three properties of the implementation are load-bearing:
 *
 * - **pdf.js is imported lazily.** The library is ~450 KB and its worker ~1.2 MB;
 *   most sessions never open a PDF, so the import happens on the first document
 *   and the module is memoised process-wide afterwards.
 * - **Canvases are created and destroyed as they scroll.** iOS caps the total
 *   canvas backing store a page may hold (a few hundred MB) and *silently blanks*
 *   canvases once past it — so a 200-page document rendered eagerly would come
 *   out empty on exactly the platform this component was written for. Only pages
 *   near the viewport hold pixels; the rest are placeholder boxes of the right
 *   size, which is also what keeps the scrollbar honest.
 * - **The text layer is best-effort.** It is what makes selection and ⌘F work,
 *   but it is transparent DOM sitting on top of the pixels: if it fails, the
 *   page is still perfectly readable, so its errors are swallowed rather than
 *   surfaced.
 *
 * The page boxes live in `.pdfv-pages`, which the Lit template declares empty
 * and with no bindings inside — that is deliberate, and the one rule to keep
 * when editing `render()`: Lit only manages nodes around its own markers, so a
 * binding placed in there would have it clobber the canvases we append by hand.
 *
 * pdf.js 6 uses `Promise.withResolvers` on both the main thread and inside the
 * worker, so it needs Safari/iOS 17.4+ (Chrome 119+). A main-thread-only
 * polyfill would not help — the worker is its own global scope.
 */

const PDFJS_MODULE   = '/vendor/pdf.min.mjs';
const WORKER_URL     = '/vendor/pdf.worker.min.mjs';
const STD_FONTS_URL  = '/vendor/pdf-standard-fonts/';

/** Zoom multipliers applied on top of fit-width. Index into this, never free-form. */
const ZOOM_STEPS = [0.5, 0.75, 1, 1.25, 1.5, 2, 3];
const ZOOM_FIT   = 2; // index of 1.0 — the default

/** At zoom 1 a page is fit-width, but never wider than this — a full-bleed page
 *  on a 27" monitor is a worse read than a bounded column. */
const MAX_FIT_WIDTH = 1000;

/** How far outside the viewport (in viewport heights) a page still holds pixels. */
const RENDER_MARGIN = 1.5;

/** Upper bound on a single canvas' backing store. Above ~2 the extra pixels are
 *  invisible, and on iOS they are the difference between rendering and blanking. */
const MAX_DPR = 2;

let _pdfjsPromise = null;

/** Import pdf.js once per page load and point it at the vendored worker. */
function loadPdfjs() {
  if (!_pdfjsPromise) {
    _pdfjsPromise = import(PDFJS_MODULE).then((mod) => {
      mod.GlobalWorkerOptions.workerSrc = WORKER_URL;
      return mod;
    }).catch((e) => {
      _pdfjsPromise = null;   // let a later open retry a transient network failure
      throw e;
    });
  }
  return _pdfjsPromise;
}

export class PdfView extends LightElement {
  static properties = {
    src:      { type: String },
    _error:   { state: true },
    _loading: { state: true },
    _zoomIdx: { state: true },
    _total:   { state: true },
    _current: { state: true },
  };

  constructor() {
    super();
    this.src      = null;
    this._error   = null;
    this._loading = false;
    this._zoomIdx = ZOOM_FIT;
    this._total   = 0;
    this._current = 1;

    this._doc      = null;   // PDFDocumentProxy
    this._pdfjs    = null;   // the imported module
    this._slots    = [];     // per page: { el, page, viewport, canvas, task, rendered }
    this._observer = null;   // IntersectionObserver driving render/release
    this._loadSeq  = 0;      // guards against a stale document landing after a newer one
    this._ro       = null;   // ResizeObserver on the scroll host
    this._resizeTimer = null;
    this._lastWidth   = 0;
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    this._teardown();
  }

  updated(changed) {
    if (changed.has('src')) this._open(this.src);
  }

  // ── Document lifecycle ─────────────────────────────────────────────────────

  async _open(src) {
    this._teardown();
    if (!src) return;
    const seq = ++this._loadSeq;
    this._loading = true;
    this._error   = null;
    this._total   = 0;
    this._current = 1;
    try {
      const pdfjs = await loadPdfjs();
      const doc   = await pdfjs.getDocument({
        url:                 src,
        standardFontDataUrl: STD_FONTS_URL,
      }).promise;
      // A newer src landed while we were loading — drop this one on the floor.
      if (seq !== this._loadSeq) { doc.destroy(); return; }
      this._pdfjs = pdfjs;
      this._doc   = doc;
      this._total = doc.numPages;
      this._loading = false;
      await this.updateComplete;          // the scroll host must exist to fill it
      if (seq !== this._loadSeq) return;
      await this._buildSlots(seq);
    } catch (e) {
      if (seq !== this._loadSeq) return;
      this._loading = false;
      this._error   = e?.message || String(e);
    }
  }

  /**
   * Create one placeholder box per page, sized from that page's own viewport, and
   * hand them to the IntersectionObserver. Every page is measured up front (a
   * cheap metadata call) rather than assuming page 1's aspect ratio: a document
   * that mixes portrait and landscape would otherwise resize boxes under the
   * user's finger as they scroll, which is exactly the jitter this viewer is
   * meant to remove.
   */
  async _buildSlots(seq) {
    const host  = this.querySelector('.pdfv-scroll');
    const pages = this.querySelector('.pdfv-pages');
    if (!host || !pages) return;
    pages.replaceChildren();
    this._slots = [];

    for (let n = 1; n <= this._doc.numPages; n++) {
      const page = await this._doc.getPage(n);
      if (seq !== this._loadSeq) return;
      const el = document.createElement('div');
      el.className = 'pdfv-page';
      el.dataset.page = String(n);
      pages.appendChild(el);
      this._slots.push({ el, page, viewport: page.getViewport({ scale: 1 }), canvas: null, task: null, rendered: false });
    }

    this._layout();
    this._observe(host);
    this._watchResize(host);
  }

  _teardown() {
    this._loadSeq++;
    this._observer?.disconnect();
    this._observer = null;
    this._ro?.disconnect();
    this._ro = null;
    if (this._resizeTimer) { clearTimeout(this._resizeTimer); this._resizeTimer = null; }
    for (const slot of this._slots) this._release(slot);
    this._slots = [];
    this.querySelector('.pdfv-pages')?.replaceChildren();
    this._doc?.destroy();
    this._doc = null;
    this._lastWidth = 0;
  }

  // ── Sizing ─────────────────────────────────────────────────────────────────

  /** CSS scale for the current container width and zoom step. */
  _scale() {
    const host = this.querySelector('.pdfv-scroll');
    const base = this._slots[0]?.viewport;
    if (!host || !base) return 1;
    // Subtract the gutter the stylesheet reserves so a fit-width page doesn't
    // overflow into a horizontal scrollbar.
    const avail = Math.max(120, Math.min(host.clientWidth - 24, MAX_FIT_WIDTH));
    return (avail / base.width) * ZOOM_STEPS[this._zoomIdx];
  }

  /**
   * Size every placeholder for the current scale and drop the pixels of the ones
   * already drawn, so they are redrawn at the new resolution. Called on first
   * build, on zoom, and on a settled container resize.
   *
   * A slot with a render still *in flight* is released too, not just a finished
   * one: that draw was set up against the old scale, and letting it land would
   * paint a canvas of the previous size into a box that has just been resized.
   *
   * `sweep: false` is for callers that adjust `scrollTop` afterwards — sweeping
   * first would pick the pages visible at the old offset.
   */
  _layout({ sweep = true } = {}) {
    const scale = this._scale();
    if (!(scale > 0)) return;
    for (const slot of this._slots) {
      const vp = slot.page.getViewport({ scale });
      slot.el.style.width  = `${Math.floor(vp.width)}px`;
      slot.el.style.height = `${Math.floor(vp.height)}px`;
      slot.el.style.setProperty('--scale-factor', String(scale));
      if (slot.rendered || slot.task) this._release(slot);
    }
    if (sweep) this._sweep();
  }

  _watchResize(host) {
    if (!('ResizeObserver' in window)) return;
    this._lastWidth = host.clientWidth;
    this._ro = new ResizeObserver(() => {
      // Only width changes the layout; a height change (mobile URL bar, keyboard)
      // must not trigger a full re-render of every visible page.
      if (host.clientWidth === this._lastWidth) return;
      this._lastWidth = host.clientWidth;
      if (this._resizeTimer) clearTimeout(this._resizeTimer);
      this._resizeTimer = setTimeout(() => { this._resizeTimer = null; this._layout(); }, 150);
    });
    this._ro.observe(host);
  }

  // ── Render / release as pages scroll ───────────────────────────────────────

  _observe(host) {
    this._observer?.disconnect();
    this._observer = new IntersectionObserver((entries) => {
      for (const entry of entries) {
        const slot = this._slots[Number(entry.target.dataset.page) - 1];
        if (!slot) continue;
        if (entry.isIntersecting) this._render(slot);
        else                      this._release(slot);
      }
      this._updateCurrent();
    }, { root: host, rootMargin: `${RENDER_MARGIN * 100}% 0px` });
    for (const slot of this._slots) this._observer.observe(slot.el);
  }

  /** Re-evaluate visibility without waiting for a scroll (after zoom/resize). */
  _sweep() {
    const host = this.querySelector('.pdfv-scroll');
    if (!host || !this._slots.length) return;
    const top    = host.scrollTop - host.clientHeight * RENDER_MARGIN;
    const bottom = host.scrollTop + host.clientHeight * (1 + RENDER_MARGIN);
    for (const slot of this._slots) {
      const a = slot.el.offsetTop;
      const b = a + slot.el.offsetHeight;
      if (b >= top && a <= bottom) this._render(slot);
    }
    this._updateCurrent();
  }

  /** The page occupying the middle of the viewport — what the counter reports. */
  _updateCurrent() {
    const host = this.querySelector('.pdfv-scroll');
    if (!host) return;
    const mid = host.scrollTop + host.clientHeight / 2;
    for (const slot of this._slots) {
      if (slot.el.offsetTop + slot.el.offsetHeight >= mid) {
        const n = Number(slot.el.dataset.page);
        if (n !== this._current) this._current = n;
        return;
      }
    }
  }

  async _render(slot) {
    if (slot.rendered || slot.task) return;
    const scale = this._scale();
    if (!(scale > 0)) return;
    const viewport = slot.page.getViewport({ scale });
    const dpr      = Math.min(window.devicePixelRatio || 1, MAX_DPR);

    const canvas = document.createElement('canvas');
    canvas.className   = 'pdfv-canvas';
    canvas.width       = Math.floor(viewport.width  * dpr);
    canvas.height      = Math.floor(viewport.height * dpr);
    canvas.style.width  = `${Math.floor(viewport.width)}px`;
    canvas.style.height = `${Math.floor(viewport.height)}px`;
    slot.el.appendChild(canvas);
    slot.canvas = canvas;

    const ctx = canvas.getContext('2d', { alpha: false });
    slot.task = slot.page.render({
      canvasContext: ctx,
      viewport,
      transform: dpr === 1 ? null : [dpr, 0, 0, dpr, 0, 0],
    });
    try {
      await slot.task.promise;
      slot.task     = null;
      slot.rendered = true;
      await this._renderText(slot, viewport);
    } catch (e) {
      slot.task = null;
      // RenderingCancelledException is the normal outcome of scrolling away or
      // zooming mid-draw — _release already tore the canvas down.
      if (e?.name !== 'RenderingCancelledException') slot.el.classList.add('pdfv-page-failed');
    }
  }

  /** Transparent selectable text over the pixels. Failure is cosmetic — swallow it. */
  async _renderText(slot, viewport) {
    try {
      const container = document.createElement('div');
      container.className = 'textLayer';
      const layer = new this._pdfjs.TextLayer({
        textContentSource: slot.page.streamTextContent(),
        container,
        viewport,
      });
      await layer.render();
      if (slot.rendered) slot.el.appendChild(container);
    } catch { /* selection is a bonus; the page is already readable */ }
  }

  /** Drop a page's pixels. Zeroing the canvas first is what actually frees the
   *  backing store on WebKit — removing the element alone is not enough. */
  _release(slot) {
    slot.task?.cancel();
    slot.task = null;
    if (slot.canvas) {
      slot.canvas.width  = 0;
      slot.canvas.height = 0;
      slot.canvas.remove();
      slot.canvas = null;
    }
    slot.el.querySelector('.textLayer')?.remove();
    slot.el.classList.remove('pdfv-page-failed');
    slot.rendered = false;
  }

  // ── Toolbar ────────────────────────────────────────────────────────────────

  _zoom(delta) {
    const next = Math.max(0, Math.min(ZOOM_STEPS.length - 1, this._zoomIdx + delta));
    if (next === this._zoomIdx) return;
    // Keep the page under the middle of the viewport in place across the zoom.
    const host   = this.querySelector('.pdfv-scroll');
    const anchor = this._current;
    this._zoomIdx = next;
    this.updateComplete.then(() => {
      this._layout({ sweep: false });
      const slot = this._slots[anchor - 1];
      if (host && slot) host.scrollTop = slot.el.offsetTop - 8;
      this._sweep();
    });
  }

  _onScroll() {
    // The observer drives rendering; this only keeps the page counter live
    // (scrolling within one tall page fires no intersection change).
    this._updateCurrent();
  }

  render() {
    if (this._error) {
      return html`<div class="fv-state text-danger">
        <i class="bi bi-exclamation-triangle fs-3 d-block mb-2"></i>${t('fv.pdf_failed')}
        <div class="pdfv-error-detail">${this._error}</div>
      </div>`;
    }
    return html`
      <div class="pdfv">
        <div class="pdfv-toolbar">
          <button class="pdfv-btn" title=${t('fv.zoom_out')}
            ?disabled=${this._zoomIdx === 0}
            @click=${() => this._zoom(-1)}><i class="bi bi-zoom-out"></i></button>
          <span class="pdfv-zoom">${Math.round(ZOOM_STEPS[this._zoomIdx] * 100)}%</span>
          <button class="pdfv-btn" title=${t('fv.zoom_in')}
            ?disabled=${this._zoomIdx === ZOOM_STEPS.length - 1}
            @click=${() => this._zoom(1)}><i class="bi bi-zoom-in"></i></button>
          ${this._total
            ? html`<span class="pdfv-pageno">${this._current} / ${this._total}</span>`
            : nothing}
        </div>
        <div class="pdfv-scroll" @scroll=${this._onScroll}>
          <!-- Filled imperatively — must stay free of Lit bindings (see the header). -->
          <div class="pdfv-pages"></div>
          ${this._loading ? html`<div class="fv-state"><span class="spinner-border"></span></div>` : nothing}
        </div>
      </div>
    `;
  }
}

customElements.define('pdf-view', PdfView);
