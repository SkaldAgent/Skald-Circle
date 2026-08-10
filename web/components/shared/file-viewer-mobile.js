import { html, nothing } from 'lit';
import { t }             from '../../lib/i18n.js';
import { FileViewerBase } from './file-viewer-base.js';

/**
 * Mobile file-viewer page. Same engine as the desktop `<file-viewer-page>`, but
 * prop-driven: `<mobile-app>` binds `visible` / `path` from its hash router
 * (`#file_viewer?path=...`) instead of the component listening to the hash. The
 * back button returns to the previous mobile section via history.
 */
export class MobileFileViewerPage extends FileViewerBase {
  static properties = {
    visible: { type: Boolean },
    path:    { type: String },
  };

  constructor() {
    super();
    this.visible = false;
    this.path    = null;
  }

  updated(changed) {
    if (changed.has('visible') || changed.has('path')) {
      if (this.visible && this.path) this._show(this.path);
      else if (!this.visible)        this._hide();
    }
  }

  _back() {
    history.back();
  }

  // Filename portion of the path, for the compact mobile header title.
  _basename() {
    const p = this.path || '';
    const i = p.lastIndexOf('/');
    return i < 0 ? p : p.slice(i + 1);
  }

  render() {
    if (!this.visible) return nothing;
    return html`
      <div class="mobile-file-viewer">
        <div class="mobile-section-header">
          <span class="mobile-section-title">
            <button class="chat-page-back" title=${t('fv.back')} @click=${() => this._back()}>
              <i class="bi bi-arrow-left"></i>
            </button>
            <span class="fv-mobile-name" title=${this.path ?? ''}><bdi>${this._basename()}</bdi></span>
          </span>
          <span class="fv-header-actions">
            ${this._renderHistoryButton('chat-page-back')}
            ${this._renderModeToggle('chat-page-back')}
            <button class="chat-page-back" title=${t('fv.download')} @click=${() => this._download()}>
              <i class="bi bi-download"></i>
            </button>
          </span>
        </div>
        ${this._renderVersionBanner()}
        <div class="fv-body">${this._renderBody()}</div>
      </div>
    `;
  }
}

customElements.define('mobile-file-viewer-page', MobileFileViewerPage);
