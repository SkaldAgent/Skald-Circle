import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';
import { t }            from '../lib/i18n.js';
import { jf, hasSchema, pluginHealth } from './shared/plugin-common.js';

// Plugins page (`#plugins`) — the admin board of every registered plugin,
// and the single plugin management surface (the old per-user `#plugins`
// page is gone: a plugin with per-user settings — Telegram's pairing,
// Honcho's opt-in — hosts them in its own sidebar page via `web_pages()`).
//
// One card per plugin: an enable/disable toggle, a health dot (green =
// enabled, running and fully configured; red = enabled but broken; grey =
// off) and a Configure button opening the plugin's own detail page
// (`#plugin-detail?id=…`). Instance config and user-access grants live on the
// detail page, not here — the catalog stays a quick status board.
//
// Styling reuses the connectors card grid (`web/css/connectors.css`).

const PAGE_ID = 'plugins';

export class PluginCatalogPage extends LightElement {

  static get properties() {
    return {
      _open:   { state: true },
      _all:    { state: true },   // PluginInfo[]
      _error:  { state: true },
      _status: { state: true },   // { [pluginId]: { err?: string } } — toggle feedback
    };
  }

  constructor() {
    super();
    this._open = false;
    this._reset();
  }

  _reset() {
    this._all = null;
    this._error = null;
    this._status = {};
  }

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === PAGE_ID;
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) this._load();
    });
  }

  disconnectedCallback() {
    window.removeEventListener('locale-changed', this.__onLocaleChanged);
    super.disconnectedCallback();
  }

  async _load() {
    this._error = null;
    try {
      this._all = await jf('/api/plugins');
    } catch (e) {
      this._error = e.message;
    }
  }

  /// The toggle flips `enabled` only — the persisted config travels back
  /// unchanged so a flip never clobbers what the detail page saved.
  async _toggle(p, enabled) {
    this._status = { ...this._status, [p.id]: {} };
    try {
      await jf(`/api/plugins/${encodeURIComponent(p.id)}`, {
        method: 'PUT', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ enabled, config: p.config || {} }),
      });
      this._all = await jf('/api/plugins');
      window.dispatchEvent(new CustomEvent('plugins-changed'));
    } catch (e) {
      this._status = { ...this._status, [p.id]: { err: e.message } };
    }
  }

  _configure(p) {
    history.pushState({ page: 'plugin-detail' }, '', `#plugin-detail?id=${encodeURIComponent(p.id)}`);
    window.dispatchEvent(new CustomEvent('llm-page-change', { detail: { page: 'plugin-detail' } }));
  }

  // ── Render ─────────────────────────────────────────────────────────────────

  render() {
    if (!this._open) return nothing;
    const loading = this._all === null && !this._error;

    return html`
      <div class="um-page">
        <div class="um-header">
          <h2 class="um-title"><i class="bi bi-puzzle-fill me-2"></i>${t('nav.plugins')}</h2>
        </div>

        ${this._error ? html`
          <div class="alert alert-danger py-2 mx-4" style="font-size:.85rem">${this._error}</div>` : nothing}

        ${loading
          ? html`<div class="um-empty"><i class="bi bi-hourglass-split"></i> ${t('plugins.loading')}</div>`
          : html`
            <div style="padding:0 1.25rem 1.5rem; overflow:auto">
              ${(this._all ?? []).length === 0
                ? html`<div class="um-empty" style="padding:1rem"><i class="bi bi-puzzle"></i><p>${t('plugins.empty.manage')}</p></div>`
                : html`<div class="connector-grid">${this._all.map(p => this._renderCard(p))}</div>`}
            </div>`}
      </div>`;
  }

  _renderHealth(p) {
    const h = pluginHealth(p);
    const cls = h === 'ok' ? 'ok' : (h === 'off' ? 'off' : 'err');
    return html`
      <span class="d-inline-flex align-items-center gap-1" style="font-size:.72rem;color:var(--placeholder-color)">
        <span class="plugin-status-dot plugin-status-dot--${cls}"></span>
        ${t(`plugins.health.${h}`)}
      </span>`;
  }

  _renderCard(p) {
    const status = this._status[p.id] || {};
    return html`
      <div class="connector-card" style="cursor:default">
        <div class="connector-card-head">
          <div class="connector-card-icon connector-card-icon--empty"><i class="bi bi-puzzle"></i></div>
          <div class="connector-card-title">
            <div class="connector-card-name">${p.name}</div>
            <div class="connector-card-sub">${p.id}</div>
          </div>
          ${this._renderHealth(p)}
        </div>
        ${p.description ? html`<div class="connector-card-desc">${p.description}</div>` : nothing}

        <div class="connector-chips">
          ${hasSchema(p.config_schema) ? html`
            <span class="connector-chip"><i class="bi bi-sliders"></i>${t('plugins.badge.instance_config')}</span>` : nothing}
          ${p.has_user_page ? html`
            <span class="connector-chip connector-chip--scope"><i class="bi bi-person"></i>${t('plugins.badge.user_page')}</span>` : nothing}
        </div>

        <div class="d-flex align-items-center justify-content-between mt-1">
          <div class="form-check form-switch mb-0">
            <input class="form-check-input" type="checkbox" role="switch" id="plugin-on-${p.id}"
              .checked=${p.enabled}
              @change=${(e) => this._toggle(p, e.target.checked)} />
            <label class="form-check-label" for="plugin-on-${p.id}" style="font-size:.82rem">${t('plugins.enabled')}</label>
          </div>
          <button class="btn btn-sm btn-outline-secondary" @click=${() => this._configure(p)}>
            <i class="bi bi-gear me-1"></i>${t('plugins.catalog.configure')}
          </button>
        </div>

        ${status.err ? html`<div class="alert alert-danger py-1 px-2" style="font-size:.78rem">${status.err}</div>` : nothing}
      </div>`;
  }
}
