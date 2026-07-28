// Telegram pairing page (page_id `telegram`, visible to any user with a
// `plugin_access` grant).
//
// Self-service chat linking: the user sends any message to the bot, gets a
// 6-character code back, and pastes it here. Reuses the core per-user config
// endpoints — `GET /api/plugins/mine` to read the `{linked, chat_id}` status
// blob, `PUT /api/plugins/telegram/my-config` to submit the code (the
// plugin's `update_user_config` override turns it into a chat↔user binding) —
// so this fragment needs no backend of its own. Default-exports the element
// class; the host registers it.
import { LitElement, html, nothing } from 'lit';
import { t, addStrings, I18nMixin } from '/lib/i18n.js';
import STRINGS from './i18n.js';

addStrings(STRINGS);

const P = 'plugin.telegram';
const ID = 'telegram';

/// JSON fetch that throws the server's error text on non-2xx and tolerates an
/// empty (204) body. The server's error text is already localized, so it is
/// safe to surface directly.
async function jf(url, opts = {}) {
  const res = await fetch(url, {
    headers: { 'Content-Type': 'application/json', ...(opts.headers || {}) },
    ...opts,
  });
  if (!res.ok) {
    const txt = await res.text().catch(() => '');
    throw new Error(txt || `HTTP ${res.status}`);
  }
  if (res.status === 204) return null;
  const ct = res.headers.get('content-type') || '';
  return ct.includes('application/json') ? res.json() : res.text();
}

export default class TelegramPage extends I18nMixin(LitElement) {
  // Light DOM, so Bootstrap classes and the app's theme CSS variables apply.
  createRenderRoot() { return this; }

  static get properties() {
    return {
      _row:     { state: true },   // UserPluginView | null (null once loaded = not granted)
      _code:    { state: true },   // pairing code draft
      _status:  { state: true },   // { ok?, err? }
      _error:   { state: true },
      _loading: { state: true },
    };
  }

  constructor() {
    super();
    this._row = null;
    this._code = '';
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
      this._row = (mine ?? []).find(x => x.id === ID) ?? null;
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
        body: JSON.stringify({ pairing_code: this._code.trim() }),
      });
      this._code = '';
      this._status = { ok: t(`${P}.saved`) };
      await this._load();
    } catch (e) {
      this._status = { err: e.message };
    }
  }

  render() {
    return html`
      <div class="um-page">
        <div class="um-header">
          <h2 class="um-title"><i class="bi bi-telegram me-2"></i>${t(`${P}.title`)}</h2>
        </div>
        <div style="padding:0 1.25rem 2rem; max-width:640px; overflow:auto">
          ${this._error ? html`<div class="alert alert-danger py-2" style="font-size:.85rem">${this._error}</div>` : nothing}
          ${this._loading
            ? html`<div class="um-empty"><i class="bi bi-hourglass-split"></i> ${t(`${P}.loading`)}</div>`
            : this._row ? this._renderBody() : this._renderUnavailable()}
        </div>
      </div>`;
  }

  _renderUnavailable() {
    return html`
      <div class="um-empty" style="padding:1rem">
        <i class="bi bi-shield-lock"></i>
        <p>${t(`${P}.unavailable`)}</p>
      </div>`;
  }

  _renderBody() {
    const linked = !!this._row?.user_config?.linked;
    const chatId = this._row?.user_config?.chat_id;
    return html`
      <p class="text-body-secondary" style="font-size:.9rem">${t(`${P}.intro`)}</p>

      <div class="connector-card" style="cursor:default">
        <div class="connector-card-head">
          <div class="connector-card-icon connector-card-icon--empty"><i class="bi bi-telegram"></i></div>
          <div class="connector-card-title">
            <div class="connector-card-name" style="font-size:.9rem">
              ${linked ? t(`${P}.status.linked`) : t(`${P}.status.unlinked`)}
            </div>
            ${linked && chatId != null ? html`
              <div class="connector-card-sub">${t(`${P}.status.chat_id`)}: ${chatId}</div>` : nothing}
          </div>
          <span class="connector-chip ${linked ? 'connector-chip--ok' : ''}">
            ${linked ? html`<i class="bi bi-check-lg"></i>` : html`<i class="bi bi-dash-lg"></i>`}
          </span>
        </div>
      </div>

      <div class="mt-4" style="font-size:.85rem; font-weight:600">
        <i class="bi bi-link-45deg me-1"></i>${t(`${P}.howto_title`)}
      </div>
      <p class="text-body-secondary" style="font-size:.82rem">${t(`${P}.howto_body`)}</p>

      <div class="mb-3" style="max-width:280px">
        <label class="form-label" style="font-size:.8rem">${t(`${P}.code_label`)}</label>
        <input class="form-control form-control-sm" type="text" .value=${this._code}
          @input=${(e) => { this._code = e.target.value; this._status = {}; }} />
      </div>

      ${this._status.err ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:.82rem">${this._status.err}</div>` : nothing}
      ${this._status.ok ? html`<div class="alert alert-success py-2 mb-3" style="font-size:.82rem">${this._status.ok}</div>` : nothing}

      <button class="btn btn-primary btn-sm" ?disabled=${!this._code.trim()} @click=${() => this._save()}>
        <i class="bi bi-check-lg me-1"></i>${t(`${P}.save`)}
      </button>
      ${linked ? html`
        <div class="text-body-secondary mt-2" style="font-size:.75rem">${t(`${P}.relink_hint`)}</div>` : nothing}
    `;
  }
}
