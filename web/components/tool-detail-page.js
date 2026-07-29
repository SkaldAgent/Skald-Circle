import { html, nothing } from 'lit';
import { LightElement }   from '../lib/base.js';
import { t }              from '../lib/i18n.js';
import { fetchToolDetail, renderToolBody, STATUS_ICON } from './shared/tool-detail-view.js';

const PAGE_ID = 'tool_detail';

function idFromHash() {
  const h = location.hash;
  const prefix = `#${PAGE_ID}?id=`;
  if (!h.startsWith(prefix)) return null;
  try {
    return decodeURIComponent(h.slice(prefix.length));
  } catch {
    return null;
  }
}

/**
 * Desktop tool-execution detail page. Self-routes off the hash
 * (`#tool_detail?id=...`), mirroring `file-viewer-page.js`: the sidebar's
 * `llm-page-change` event toggles visibility and `hashchange` re-loads. It hydrates
 * from `GET /api/tools/{id}` (via the shared `tool-detail-view` engine) so a tool's
 * input / result / diff is readable in the center panel even after a page reload.
 */
export class ToolDetailPage extends LightElement {
  static properties = {
    _open:    { state: true },
    _loading: { state: true },
    _error:   { state: true },
    _tool:    { state: true },
  };

  constructor() {
    super();
    this._open    = false;
    this._loading = false;
    this._error   = null;
    this._tool    = null;
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === PAGE_ID;
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) this._loadFromHash();
    });
    window.addEventListener('hashchange', () => {
      if (this._open) this._loadFromHash();
    });
  }

  async _loadFromHash() {
    const id = idFromHash();
    if (id == null) return;
    this._loading = true;
    this._error = null;
    this._tool = null;
    try {
      this._tool = await fetchToolDetail(id);
    } catch (e) {
      this._error = e.message || String(e);
    } finally {
      this._loading = false;
    }
  }

  _back() { history.back(); }

  render() {
    if (!this._open) return nothing;
    const tl = this._tool;
    const si = tl ? (STATUS_ICON[tl.status] || STATUS_ICON.done) : null;
    return html`
      <div class="llm-page tool-detail-page">
        <div class="page-header">
          <div class="page-header-left">
            <button class="btn btn-sm btn-outline-secondary page-header-back" title=${t('fv.back')} @click=${() => this._back()}>
              <i class="bi bi-arrow-left"></i>
            </button>
            <h2 class="page-header-title">
              ${tl ? html`
                ${si ? html`<i class="bi ${si.glyph} ${si.cls} me-2"></i>` : nothing}
                ${tl.display_name || tl.name}
              ` : t('tool_detail.title')}
            </h2>
          </div>
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
