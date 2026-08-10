import { html, nothing } from 'lit';
import { t }            from '../lib/i18n.js';
import { FileViewerBase } from './shared/file-viewer-base.js';

const PAGE_ID = 'file_viewer';

function pathFromHash() {
  const h = location.hash;
  const prefix = `#${PAGE_ID}?path=`;
  if (!h.startsWith(prefix)) return null;
  try {
    return decodeURIComponent(h.slice(prefix.length));
  } catch {
    return null;
  }
}

/**
 * Desktop file-viewer page. Self-routes off the hash (`#file_viewer?path=...`):
 * the sidebar's `llm-page-change` event toggles visibility and `hashchange`
 * re-loads. All fetch/render/watch logic lives in `FileViewerBase`; this
 * subclass only adds the desktop chrome and the hash wiring.
 */
export class FileViewerPage extends FileViewerBase {
  static properties = {
    _open: { state: true },
  };

  constructor() {
    super();
    this._open = false;
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === PAGE_ID;
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) this._loadFromHash();
      else this._hide();
    });
    window.addEventListener('hashchange', () => {
      if (this._open) this._loadFromHash();
    });
  }

  _loadFromHash() {
    const path = pathFromHash();
    if (path) this._show(path);
  }

  _back() {
    history.back();
  }

  render() {
    if (!this._open) return nothing;
    return html`
      <div class="llm-page fv-page">
        <div class="page-header">
          <div class="page-header-left">
            <button class="btn btn-sm btn-outline-secondary page-header-back" title=${t('fv.back')} @click=${() => this._back()}>
              <i class="bi bi-arrow-left"></i>
            </button>
            <h2 class="page-header-title fv-title" title=${this._path ?? ''}><bdi>${this._path ?? ''}</bdi></h2>
          </div>
          <div class="fv-header-actions">
            ${this._renderHistoryButton('btn btn-sm btn-outline-secondary fv-download-btn')}
            ${this._renderModeToggle('btn btn-sm btn-outline-secondary fv-download-btn')}
            <button class="btn btn-sm btn-outline-secondary fv-download-btn" title=${t('fv.download')} @click=${() => this._download()}>
              <i class="bi bi-download"></i>
            </button>
          </div>
        </div>
        ${this._renderVersionBanner()}
        <div class="fv-body">${this._renderBody()}</div>
      </div>
    `;
  }
}
