import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';
import { t }            from '../lib/i18n.js';
import { jf, schemaFields } from './shared/plugin-common.js';

// Plugins page (`#plugins`) — the user-facing half of the plugin split.
//
// Shows the plugins the caller has been granted (`plugin_access`, admin-granted).
// When a plugin declares a `user_config_schema` the card carries a small
// schema-driven form — e.g. Telegram's pairing code — saved via
// `PUT /api/plugins/{id}/my-config`.
//
// The admin half (enable/disable, instance config, access grants) lives on
// `#plugin-catalog` + `#plugin-detail` — see `plugin-catalog.js`.
//
// Styling reuses the connectors card grid (`web/css/connectors.css`).

export class PluginsPage extends LightElement {

  static get properties() {
    return {
      _open:    { state: true },
      _mine:    { state: true },   // UserPluginView[] — granted + enabled plugins
      _error:   { state: true },
      _uDrafts: { state: true },   // user config drafts: { [pluginId]: {key: value} }
      _uStatus: { state: true },   // { [pluginId]: { ok?: string, err?: string } }
    };
  }

  constructor() {
    super();
    this._open = false;
    this._reset();
  }

  _reset() {
    this._mine = null;
    this._error = null;
    this._uDrafts = {};
    this._uStatus = {};
  }

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'plugins';
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
      this._mine = await jf('/api/plugins/mine');
    } catch (e) {
      this._error = e.message;
    }
  }

  _uDraft(p) {
    if (!this._uDrafts[p.id]) {
      // Seed the form from the stored config for keys the schema knows.
      const draft = {};
      for (const f of schemaFields(p.user_config_schema)) {
        const v = p.user_config?.[f.key];
        draft[f.key] = v ?? (f.type === 'boolean' ? false : '');
      }
      this._uDrafts = { ...this._uDrafts, [p.id]: draft };
    }
    return this._uDrafts[p.id];
  }

  _setUDraft(id, key, value) {
    this._uDrafts = { ...this._uDrafts, [id]: { ...this._uDrafts[id], [key]: value } };
  }

  async _saveUserConfig(p) {
    const draft = this._uDraft(p);
    for (const f of schemaFields(p.user_config_schema)) {
      if (f.required && !draft[f.key]) {
        this._uStatus = { ...this._uStatus, [p.id]: { err: t('plugins.error.required', { field: f.label }) } };
        return;
      }
    }
    this._uStatus = { ...this._uStatus, [p.id]: {} };
    try {
      await jf(`/api/plugins/${encodeURIComponent(p.id)}/my-config`, {
        method: 'PUT', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(draft),
      });
      this._uStatus = { ...this._uStatus, [p.id]: { ok: t('plugins.saved') } };
      // Drop the draft so the reloaded status blob re-seeds the form.
      const drafts = { ...this._uDrafts };
      delete drafts[p.id];
      this._uDrafts = drafts;
      this._mine = await jf('/api/plugins/mine');
    } catch (e) {
      this._uStatus = { ...this._uStatus, [p.id]: { err: e.message } };
    }
  }

  // ── Render ─────────────────────────────────────────────────────────────────

  render() {
    if (!this._open) return nothing;
    const loading = this._mine === null && !this._error;

    return html`
      <div class="um-page">
        <div class="um-header">
          <h2 class="um-title"><i class="bi bi-puzzle me-2"></i>${t('plugins.title')}</h2>
        </div>

        ${this._error ? html`
          <div class="alert alert-danger py-2 mx-4" style="font-size:.85rem">${this._error}</div>` : nothing}

        ${loading
          ? html`<div class="um-empty"><i class="bi bi-hourglass-split"></i> ${t('plugins.loading')}</div>`
          : html`
            <div style="padding:0 1.25rem 1.5rem; overflow:auto">
              ${this._renderMine()}
            </div>`}
      </div>`;
  }

  _renderMine() {
    const rows = this._mine ?? [];
    if (rows.length === 0) {
      return html`
        <div class="um-empty" style="padding:1rem"><i class="bi bi-puzzle"></i>
          <p>${t('plugins.empty.mine')}</p>
          <p style="font-size:.8rem;opacity:.7">${t('plugins.empty.ask_admin')}</p>
        </div>`;
    }
    return html`<div class="connector-grid">${rows.map(p => this._renderUserCard(p))}</div>`;
  }

  /// Stored config entries the schema does not cover (e.g. Telegram's
  /// `{linked, chat_id}` status blob) rendered as a small status list.
  _renderUserStatus(p) {
    const covered = new Set(schemaFields(p.user_config_schema).map(f => f.key));
    const extra = Object.entries(p.user_config || {}).filter(([k]) => !covered.has(k));
    if (!extra.length) return nothing;
    return html`
      <div class="d-flex flex-column gap-1 mb-2" style="font-size:.78rem">
        ${extra.map(([k, v]) => html`
          <div class="d-flex justify-content-between">
            <span class="text-muted">${k}</span>
            <span>${typeof v === 'boolean' ? (v ? t('plugins.yes') : t('plugins.no')) : String(v)}</span>
          </div>`)}
      </div>`;
  }

  _renderUserCard(p) {
    const fields = schemaFields(p.user_config_schema);
    const status = this._uStatus[p.id] || {};
    const draft = fields.length ? this._uDraft(p) : {};
    return html`
      <div class="connector-card" style="cursor:default">
        <div class="connector-card-head">
          <div class="connector-card-icon connector-card-icon--empty"><i class="bi bi-puzzle"></i></div>
          <div class="connector-card-title">
            <div class="connector-card-name">${p.name}</div>
            <div class="connector-card-sub">${p.id}</div>
          </div>
          <span class="connector-chip connector-chip--ok">${t('plugins.status.active')}</span>
        </div>
        ${p.description ? html`<div class="connector-card-desc">${p.description}</div>` : nothing}
        ${this._renderUserStatus(p)}
        ${fields.length ? html`
          <div class="mt-2">
            ${fields.map(f => html`
              <div class="mb-2">
                <label class="form-label" style="font-size:.8rem">${f.label}${f.required ? html`<span class="text-danger">*</span>` : nothing}</label>
                ${f.type === 'boolean' ? html`
                  <div class="form-check">
                    <input class="form-check-input" type="checkbox" .checked=${!!draft[f.key]}
                      @change=${(e) => this._setUDraft(p.id, f.key, e.target.checked)} />
                  </div>` : html`
                  <input class="form-control form-control-sm"
                    type=${f.sensitive ? 'password' : (f.type === 'number' ? 'number' : 'text')}
                    .value=${String(draft[f.key] ?? '')}
                    @input=${(e) => this._setUDraft(p.id, f.key, f.type === 'number' ? Number(e.target.value) : e.target.value)} />`}
                ${f.description ? html`<div class="form-text" style="font-size:.7rem">${f.description}</div>` : nothing}
              </div>`)}
            ${status.err ? html`<div class="alert alert-danger py-1 px-2" style="font-size:.78rem">${status.err}</div>` : nothing}
            ${status.ok ? html`<div class="alert alert-success py-1 px-2" style="font-size:.78rem">${status.ok}</div>` : nothing}
            <button class="btn btn-sm btn-primary" @click=${() => this._saveUserConfig(p)}>
              <i class="bi bi-check-lg me-1"></i>${t('plugins.save')}
            </button>
          </div>` : nothing}
      </div>`;
  }
}
