import { LitElement, html, nothing } from 'lit';
import { t }              from '../../lib/i18n.js';
import { fetchToolDetail, renderToolBody, STATUS_ICON } from './tool-detail-view.js';

/**
 * Mobile tool-execution detail page. Same shared engine as the desktop
 * `<tool-detail-page>`, but prop-driven: `<mobile-app>` binds `visible` / `toolId`
 * from its hash router (`#tool_detail?id=...`) instead of the component listening to
 * the hash. The back button returns to the previous mobile section via history.
 */
export class MobileToolDetailPage extends LitElement {
  // No shadow DOM — inherit the app's global CSS + Bootstrap Icons.
  createRenderRoot() { return this; }

  static properties = {
    visible:   { type: Boolean },
    toolId:    { attribute: 'tool-id' },
    _loading:  { state: true },
    _error:    { state: true },
    _tool:     { state: true },
  };

  constructor() {
    super();
    this.visible  = false;
    this.toolId   = null;
    this._loading = false;
    this._error   = null;
    this._tool    = null;
  }

  updated(changed) {
    if (changed.has('visible') || changed.has('toolId')) {
      if (this.visible && this.toolId != null) this._load();
    }
  }

  async _load() {
    this._loading = true;
    this._error = null;
    this._tool = null;
    try {
      this._tool = await fetchToolDetail(this.toolId);
    } catch (e) {
      this._error = e.message || String(e);
    } finally {
      this._loading = false;
    }
  }

  _back() { history.back(); }

  render() {
    if (!this.visible) return nothing;
    const tl = this._tool;
    const si = tl ? (STATUS_ICON[tl.status] || STATUS_ICON.done) : null;
    return html`
      <div class="mobile-tool-detail tool-detail-page">
        <div class="mobile-section-header">
          <span class="mobile-section-title">
            <button class="chat-page-back" title=${t('fv.back')} @click=${() => this._back()}>
              <i class="bi bi-arrow-left"></i>
            </button>
            <span>
              ${si ? html`<i class="bi ${si.glyph} ${si.cls} me-1"></i>` : nothing}
              ${tl ? (tl.display_name || tl.name) : t('tool_detail.title')}
            </span>
          </span>
        </div>
        <div class="tool-detail-body">
          ${this._loading ? html`<div class="tool-detail-muted">${t('common.loading')}</div>` : nothing}
          ${this._error ? html`<div class="alert alert-danger">${this._error}</div>` : nothing}
          ${renderToolBody(tl)}
        </div>
      </div>
    `;
  }
}

customElements.define('mobile-tool-detail-page', MobileToolDetailPage);
