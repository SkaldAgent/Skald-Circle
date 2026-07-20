// Honcho user opt-in page (page_id `memory`, visible to any user with a
// `plugin_access` grant).
//
// The per-user consent to long-term memory. Reuses the core per-user config
// endpoints — `GET /api/plugins/mine` to read the current flag,
// `PUT /api/plugins/honcho/my-config` to save `{ enabled }` — so this fragment
// needs no backend of its own. Structured in sections so the future "what does
// Honcho know about me?" panel is a drop-in addition (see the `soon` section).
// Default-exports the element class; the host registers it.
import { html, nothing } from 'lit';
import { HonchoBase, jf, t } from './common.js';

const P = 'plugin.honcho';
const ID = 'honcho';

export default class HonchoMemoryPage extends HonchoBase {
  static get properties() {
    return {
      _row:     { state: true },   // UserPluginView | null (null once loaded = not granted)
      _enabled: { state: true },   // draft toggle
      _status:  { state: true },   // { ok?, err? }
      _error:   { state: true },
      _loading: { state: true },
    };
  }

  constructor() {
    super();
    this._row = null;
    this._enabled = false;
    this._status = {};
    this._error = null;
    this._loading = true;
  }

  connectedCallback() {
    super.connectedCallback();
    this._load();
  }

  async _load() {
    this._loading = true;
    this._error = null;
    try {
      const mine = await jf('/api/plugins/mine');
      const row = (mine ?? []).find(x => x.id === ID) ?? null;
      this._row = row;
      this._enabled = !!row?.user_config?.enabled;
    } catch (e) {
      this._error = e.message;
    } finally {
      this._loading = false;
    }
  }

  async _save() {
    this._status = {};
    try {
      await jf(`/api/plugins/${ID}/my-config`, {
        method: 'PUT',
        body: JSON.stringify({ enabled: this._enabled }),
      });
      this._status = { ok: t(`${P}.memory.saved`) };
      await this._load();
    } catch (e) {
      this._status = { err: e.message };
    }
  }

  render() {
    return html`
      <div class="um-page">
        <div class="um-header">
          <h2 class="um-title"><i class="bi bi-stars me-2"></i>${t(`${P}.memory.title`)}</h2>
        </div>
        <div style="padding:0 1.25rem 2rem; max-width:640px; overflow:auto">
          ${this._error ? html`<div class="alert alert-danger py-2" style="font-size:.85rem">${this._error}</div>` : nothing}
          ${this._loading
            ? html`<div class="um-empty"><i class="bi bi-hourglass-split"></i> ${t(`${P}.memory.loading`)}</div>`
            : this._row ? this._renderBody() : this._renderUnavailable()}
        </div>
      </div>`;
  }

  _renderUnavailable() {
    return html`
      <div class="um-empty" style="padding:1rem">
        <i class="bi bi-shield-lock"></i>
        <p>${t(`${P}.memory.unavailable`)}</p>
      </div>`;
  }

  _renderBody() {
    return html`
      <p class="text-body-secondary" style="font-size:.9rem">${t(`${P}.memory.intro`)}</p>

      <div class="connector-card" style="cursor:default; border-color:var(--warning, #e0a800)">
        <div class="connector-card-name" style="font-size:.9rem">
          <i class="bi bi-exclamation-triangle me-1"></i>${t(`${P}.memory.privacy_title`)}
        </div>
        <div class="connector-card-desc" style="-webkit-line-clamp:initial; margin-top:.35rem">
          ${t(`${P}.memory.privacy_body`)}
        </div>
      </div>

      <div class="form-check form-switch my-3">
        <input class="form-check-input" type="checkbox" role="switch" id="honcho-optin"
          .checked=${this._enabled} @change=${(e) => { this._enabled = e.target.checked; this._status = {}; }} />
        <label class="form-check-label" for="honcho-optin" style="font-size:.9rem">${t(`${P}.memory.toggle`)}</label>
      </div>

      ${this._status.err ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:.82rem">${this._status.err}</div>` : nothing}
      ${this._status.ok ? html`<div class="alert alert-success py-2 mb-3" style="font-size:.82rem">${this._status.ok}</div>` : nothing}

      <button class="btn btn-primary btn-sm" @click=${() => this._save()}>
        <i class="bi bi-check-lg me-1"></i>${t(`${P}.memory.save`)}
      </button>

      ${this._renderSoon()}`;
  }

  // Placeholder for the future "what does Honcho know about me?" panel. When
  // built, this section gains a button that calls a new `GET ${this.api}/whoami`
  // (opt-in-gated) and renders the returned summary; only this method + that one
  // route change.
  _renderSoon() {
    if (!this._enabled) return nothing;
    return html`
      <hr class="my-4" style="opacity:.15" />
      <div style="opacity:.7">
        <div style="font-size:.85rem; font-weight:600"><i class="bi bi-hourglass-split me-1"></i>${t(`${P}.memory.soon_title`)}</div>
        <div class="text-body-secondary" style="font-size:.82rem; margin-top:.25rem">${t(`${P}.memory.soon_body`)}</div>
      </div>`;
  }
}
