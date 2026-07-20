// Honcho admin config page (page_id `config`, admin_only).
//
// The plugin's dedicated admin surface, richer than the generic
// `#plugin-detail` form: connection config + a "Test connection" check against
// the *current draft* before saving. Persistence reuses the core plugin
// endpoints — `GET /api/plugins` to read the row, `PUT /api/plugins/honcho` to
// save `{enabled, config}` — so nothing is stored through this fragment's own
// backend. Default-exports the element class; the host registers it.
import { html, nothing } from 'lit';
import { HonchoBase, jf, t } from './common.js';

const P = 'plugin.honcho';
const ID = 'honcho';

export default class HonchoConfigPage extends HonchoBase {
  static get properties() {
    return {
      _plugin:  { state: true },   // PluginInfo | null
      _draft:   { state: true },   // { base_url, api_key, workspace_id }
      _status:  { state: true },   // { ok?, err? } for save
      _test:    { state: true },   // { busy?, ok?, err? } for the connection test
      _error:   { state: true },
      _loading: { state: true },
    };
  }

  constructor() {
    super();
    this._plugin = null;
    this._draft = { base_url: '', api_key: '', workspace_id: '' };
    this._status = {};
    this._test = {};
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
      const all = await jf('/api/plugins');
      const p = (all ?? []).find(x => x.id === ID) ?? null;
      if (!p) { this._error = t(`${P}.config.not_found`); this._plugin = null; return; }
      this._plugin = p;
      this._draft = {
        base_url:     p.config?.base_url ?? '',
        api_key:      p.config?.api_key ?? '',
        workspace_id: p.config?.workspace_id ?? '',
      };
    } catch (e) {
      this._error = e.message;
    } finally {
      this._loading = false;
    }
  }

  _set(key, value) {
    this._draft = { ...this._draft, [key]: value };
    this._status = {};
    this._test = {};
  }

  async _save(enabled) {
    this._status = {};
    if (!this._draft.base_url?.trim()) {
      this._status = { err: t(`${P}.config.required`) };
      return;
    }
    try {
      await jf(`/api/plugins/${ID}`, {
        method: 'PUT',
        body: JSON.stringify({ enabled, config: this._draft }),
      });
      this._status = { ok: t(`${P}.config.saved`) };
      await this._load();
      window.dispatchEvent(new CustomEvent('plugins-changed'));
    } catch (e) {
      this._status = { err: e.message };
    }
  }

  async _testConnection() {
    this._test = { busy: true };
    try {
      const r = await jf(`${this.api}/admin/test`, {
        method: 'POST',
        body: JSON.stringify({ base_url: this._draft.base_url, api_key: this._draft.api_key }),
      });
      this._test = { ok: t(`${P}.config.test_ok`, { n: r?.workspaces ?? 0 }) };
    } catch (e) {
      this._test = { err: e.message };
    }
  }

  render() {
    const p = this._plugin;
    return html`
      <div class="um-page">
        <div class="um-header">
          <h2 class="um-title"><i class="bi bi-stars me-2"></i>${t(`${P}.config.title`)}</h2>
        </div>
        <div style="padding:0 1.25rem 2rem; max-width:640px; overflow:auto">
          ${this._error ? html`<div class="alert alert-danger py-2" style="font-size:.85rem">${this._error}</div>` : nothing}
          ${this._loading
            ? html`<div class="um-empty"><i class="bi bi-hourglass-split"></i> ${t(`${P}.config.loading`)}</div>`
            : p ? this._renderForm(p) : nothing}
        </div>
      </div>`;
  }

  _renderForm(p) {
    const d = this._draft;
    return html`
      <p class="text-body-secondary" style="font-size:.9rem">${t(`${P}.config.intro`)}</p>

      <div class="form-check form-switch mb-3">
        <input class="form-check-input" type="checkbox" role="switch" id="honcho-enabled"
          .checked=${!!p.enabled} @change=${(e) => this._save(e.target.checked)} />
        <label class="form-check-label" for="honcho-enabled" style="font-size:.85rem">${t(`${P}.config.enabled`)}</label>
      </div>

      <div class="mb-3">
        <label class="form-label">${t(`${P}.config.base_url`)}<span class="text-danger">*</span></label>
        <input class="form-control" type="text" .value=${d.base_url}
          @input=${(e) => this._set('base_url', e.target.value)} />
        <div class="form-text" style="font-size:.72rem">${t(`${P}.config.base_url_hint`)}</div>
      </div>

      <div class="mb-3">
        <label class="form-label">${t(`${P}.config.api_key`)}</label>
        <input class="form-control" type="password" autocomplete="off" .value=${d.api_key}
          @input=${(e) => this._set('api_key', e.target.value)} />
        <div class="form-text" style="font-size:.72rem">${t(`${P}.config.api_key_hint`)}</div>
      </div>

      <div class="mb-3">
        <label class="form-label">${t(`${P}.config.workspace`)}</label>
        <input class="form-control" type="text" .value=${d.workspace_id}
          @input=${(e) => this._set('workspace_id', e.target.value)} />
        <div class="form-text" style="font-size:.72rem">${t(`${P}.config.workspace_hint`)}</div>
      </div>

      ${this._status.err ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:.82rem">${this._status.err}</div>` : nothing}
      ${this._status.ok ? html`<div class="alert alert-success py-2 mb-3" style="font-size:.82rem">${this._status.ok}</div>` : nothing}
      ${this._test.err ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:.82rem">${this._test.err}</div>` : nothing}
      ${this._test.ok ? html`<div class="alert alert-success py-2 mb-3" style="font-size:.82rem"><i class="bi bi-check-circle me-1"></i>${this._test.ok}</div>` : nothing}

      <div class="d-flex gap-2">
        <button class="btn btn-primary btn-sm" @click=${() => this._save(p.enabled)}>
          <i class="bi bi-check-lg me-1"></i>${t(`${P}.config.save`)}
        </button>
        <button class="btn btn-outline-secondary btn-sm" ?disabled=${this._test.busy}
          @click=${() => this._testConnection()}>
          <i class="bi bi-plug me-1"></i>${this._test.busy ? t(`${P}.config.testing`) : t(`${P}.config.test`)}
        </button>
      </div>`;
  }
}
