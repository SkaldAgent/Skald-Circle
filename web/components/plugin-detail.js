import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';
import { t }            from '../lib/i18n.js';
import { jf, schemaFields, hasSchema, pluginHealth } from './shared/plugin-common.js';

// One plugin's admin page (`#plugin-detail?id=<plugin id>`), reached from the
// Configure button on `#plugin-catalog` — the plugin counterpart of
// `connector-detail.js`.
//
// Hosts what was squeezed into the old combined page: the instance-wide
// config form (`config_schema`, saved via `PUT /api/plugins/{id}`) and the
// per-user access checklist (`GET/PUT /api/plugins/{id}/access`). The enable
// toggle is repeated in the summary card so a full setup round-trip happens
// on one page.

const PAGE_ID = 'plugin-detail';

function idFromHash() {
  const m = location.hash.match(/^#plugin-detail\?id=(.*)$/);
  if (!m) return null;
  try { return decodeURIComponent(m[1]); } catch { return null; }
}

export class PluginDetailPage extends LightElement {

  static get properties() {
    return {
      _open:        { state: true },
      _id:          { state: true },
      _plugin:      { state: true },   // PluginInfo
      _customPage:  { state: true },   // this plugin's own admin page, or null
      _error:       { state: true },
      _draft:       { state: true },   // config form draft
      _status:      { state: true },   // { ok?: string, err?: string }
      _access:      { state: true },   // AccessEntry[]
      _accessSel:   { state: true },   // Set of granted user ids
      _accessErr:   { state: true },
      _accessSaved: { state: true },
    };
  }

  constructor() {
    super();
    this._open = false;
    this._reset();
  }

  _reset() {
    this._id = null;
    this._plugin = null;
    this._customPage = null;
    this._error = null;
    this._draft = null;
    this._status = {};
    this._access = null;
    this._accessSel = new Set();
    this._accessErr = null;
    this._accessSaved = false;
  }

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === PAGE_ID;
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) this._loadFromHash();
    });
    window.addEventListener('hashchange', () => {
      if (this._open) this._loadFromHash();
    });
  }

  disconnectedCallback() {
    window.removeEventListener('locale-changed', this.__onLocaleChanged);
    super.disconnectedCallback();
  }

  async _loadFromHash() {
    const id = idFromHash();
    if (!id) return;
    // A different plugin must not inherit the previous one's typed config.
    if (id !== this._id) this._reset();
    this._id = id;
    await this._load();
  }

  async _load() {
    this._error = null;
    try {
      const all = await jf('/api/plugins');
      const p = (all ?? []).find(x => x.id === this._id) ?? null;
      if (!p) {
        this._plugin = null;
        this._error = t('plugins.detail.not_found', { id: this._id });
        return;
      }
      this._plugin = p;
      // If the plugin ships its own page(s), the generic config form may defer
      // to them — see `_renderConfig`. Prefer an `admin_only` console page;
      // otherwise any page of this plugin will do (e.g. mobile-connector's
      // Mobile App page, which hosts its own settings dialog).
      try {
        const pages = await jf('/api/plugins/pages');
        const mine = (pages ?? []).filter(pg => pg.plugin_id === this._id);
        this._customPage = mine.find(pg => pg.admin_only) ?? mine[0] ?? null;
      } catch { this._customPage = null; }
      // Keep whatever the admin has already typed across a reload triggered by a save.
      this._draft = { ...(p.config || {}), ...(this._draft || {}) };
      // Binding-managed plugins (e.g. mobile-connector) gate access through
      // their own pairing lifecycle — the generic checklist controls nothing.
      if (!p.manages_own_access) await this._loadAccess();
    } catch (e) {
      this._error = e.message;
    }
  }

  async _loadAccess() {
    try {
      const entries = await jf(`/api/plugins/${encodeURIComponent(this._id)}/access`);
      this._access = entries;
      this._accessSel = new Set(entries.filter(e => e.granted).map(e => e.user_id));
    } catch (e) {
      this._accessErr = e.message;
    }
  }

  _back() {
    // Prefer real history so the browser's own Back stays consistent; fall back
    // to the catalog when this page was opened straight from a pasted URL.
    if (history.length > 1) { history.back(); return; }
    history.pushState({ page: 'plugin-catalog' }, '', '#plugin-catalog');
    window.dispatchEvent(new CustomEvent('llm-page-change', { detail: { page: 'plugin-catalog' } }));
  }

  _setDraft(key, value) {
    this._draft = { ...this._draft, [key]: value };
  }

  async _save(enabled) {
    this._status = {};
    const fields = schemaFields(this._plugin.config_schema);
    for (const f of fields) {
      if (f.required && !this._draft[f.key]) {
        this._status = { err: t('plugins.error.required', { field: f.label }) };
        return;
      }
    }
    try {
      await jf(`/api/plugins/${encodeURIComponent(this._id)}`, {
        method: 'PUT', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ enabled, config: this._draft || {} }),
      });
      this._status = { ok: t('plugins.saved') };
      this._draft = null;   // re-seed from the persisted config
      await this._load();
      window.dispatchEvent(new CustomEvent('plugins-changed'));
    } catch (e) {
      this._status = { err: e.message };
    }
  }

  _toggleAccessUser(userId, on) {
    const next = new Set(this._accessSel);
    if (on) next.add(userId); else next.delete(userId);
    this._accessSel = next;
    this._accessSaved = false;
  }

  async _saveAccess() {
    this._accessErr = null;
    this._accessSaved = false;
    try {
      await jf(`/api/plugins/${encodeURIComponent(this._id)}/access`, {
        method: 'PUT', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ user_ids: [...this._accessSel] }),
      });
      this._accessSaved = true;
    } catch (e) {
      this._accessErr = e.message;
    }
  }

  // ── Render ─────────────────────────────────────────────────────────────────

  render() {
    if (!this._open) return nothing;
    if (this._error && !this._plugin) {
      return html`
        <div class="um-page">
          ${this._renderHeader()}
          <div class="alert alert-danger py-2 mx-4" style="font-size:.85rem">${this._error}</div>
        </div>`;
    }
    if (!this._plugin) {
      return html`<div class="um-page">${this._renderHeader()}
        <div class="um-empty"><i class="bi bi-hourglass-split"></i> ${t('plugins.loading')}</div></div>`;
    }
    return html`
      <div class="um-page">
        ${this._renderHeader()}
        <div style="padding:0 1.25rem 2rem; overflow:auto">
          ${this._renderSummary()}
          ${this._renderConfig()}
          ${this._plugin.manages_own_access ? nothing : this._renderAccess()}
        </div>
      </div>`;
  }

  _renderHeader() {
    return html`
      <div class="um-header">
        <div class="d-flex align-items-center gap-2" style="min-width:0">
          <button class="btn btn-sm btn-outline-secondary" title=${t('plugins.detail.back')} @click=${() => this._back()}>
            <i class="bi bi-arrow-left"></i>
          </button>
          <h2 class="um-title" style="min-width:0;overflow:hidden;text-overflow:ellipsis">
            ${this._plugin?.name || this._id || 'Plugin'}
          </h2>
        </div>
      </div>`;
  }

  _renderSummary() {
    const p = this._plugin;
    const h = pluginHealth(p);
    const cls = h === 'ok' ? 'ok' : (h === 'off' ? 'off' : 'err');
    return html`
      <div class="connector-card" style="margin-top:1rem;cursor:default">
        <div class="connector-card-head">
          <div class="connector-card-icon connector-card-icon--empty" style="width:44px;height:44px">
            <i class="bi bi-puzzle"></i>
          </div>
          <div class="connector-card-title">
            <div class="connector-card-name" style="font-size:1rem">${p.name}</div>
            <div class="connector-card-sub">${p.id}</div>
          </div>
          <span class="d-inline-flex align-items-center gap-1" style="font-size:.72rem;color:var(--placeholder-color)">
            <span class="plugin-status-dot plugin-status-dot--${cls}"></span>
            ${t(`plugins.health.${h}`)}
          </span>
        </div>
        ${p.description ? html`<div class="connector-card-desc" style="-webkit-line-clamp:initial">${p.description}</div>` : nothing}
        <div class="connector-chips">
          ${hasSchema(p.user_config_schema) ? html`
            <span class="connector-chip connector-chip--scope"><i class="bi bi-person"></i>${t('plugins.badge.user_config')}</span>` : nothing}
        </div>
        <div class="form-check form-switch mt-1 mb-0">
          <input class="form-check-input" type="checkbox" role="switch" id="plugin-detail-on"
            .checked=${p.enabled}
            @change=${(e) => this._save(e.target.checked)} />
          <label class="form-check-label" for="plugin-detail-on" style="font-size:.82rem">${t('plugins.enabled')}</label>
        </div>
      </div>`;
  }

  _openCustomPage(e, route) {
    e.preventDefault();
    history.pushState({ page: route }, '', '#' + route);
    window.dispatchEvent(new CustomEvent('llm-page-change', { detail: { page: route } }));
  }

  _renderConfigLink() {
    const cp = this._customPage;
    const route = `plugin/${cp.plugin_id}/${cp.page_id}`;
    return html`
      <div style="margin-top:1.5rem">
        <div class="um-header" style="padding:0 0 .5rem">
          <h3 class="um-title" style="font-size:1rem"><i class="bi bi-sliders me-2"></i>${t('plugins.detail.config.title')}</h3>
        </div>
        <div class="text-muted mb-2" style="font-size:.82rem">${t('plugins.detail.config.custom_page')}</div>
        <a class="btn btn-sm btn-primary" href="#${route}" @click=${(e) => this._openCustomPage(e, route)}>
          <i class="bi bi-box-arrow-up-right me-1"></i>${t('plugins.detail.config.open')}
        </a>
      </div>`;
  }

  _renderConfig() {
    const p = this._plugin;
    const fields = schemaFields(p.config_schema);
    // The plugin hosts its own config UI in one of its pages (e.g. the mobile
    // connector's settings dialog): link out instead of duplicating the form.
    if (p.config_in_detail_page === false) {
      return this._customPage ? this._renderConfigLink() : nothing;
    }
    // Defer to the plugin's own admin page only when there is no generic
    // instance-config to show.
    if (fields.length === 0 && this._customPage) return this._renderConfigLink();
    const draft = this._draft || {};
    return html`
      <div style="margin-top:1.5rem">
        <div class="um-header" style="padding:0 0 .5rem">
          <h3 class="um-title" style="font-size:1rem"><i class="bi bi-sliders me-2"></i>${t('plugins.detail.config.title')}</h3>
        </div>
        ${fields.length === 0 ? html`
          <div class="text-muted" style="font-size:.82rem">${t('plugins.detail.config.empty')}</div>` : html`
          ${fields.map(f => html`
            <div class="mb-3">
              <label class="form-label">${f.label}${f.required ? html`<span class="text-danger">*</span>` : nothing}</label>
              ${f.type === 'boolean' ? html`
                <div class="form-check">
                  <input class="form-check-input" type="checkbox" .checked=${!!draft[f.key]}
                    @change=${(e) => this._setDraft(f.key, e.target.checked)} />
                </div>` : html`
                <input class="form-control"
                  type=${f.sensitive ? 'password' : (f.type === 'number' ? 'number' : 'text')}
                  .value=${String(draft[f.key] ?? '')}
                  @input=${(e) => this._setDraft(f.key, f.type === 'number' ? Number(e.target.value) : e.target.value)} />`}
              ${f.description ? html`<div class="form-text" style="font-size:.72rem">${f.description}</div>` : nothing}
            </div>`)}
          ${this._status.err ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:.82rem">${this._status.err}</div>` : nothing}
          ${this._status.ok ? html`<div class="alert alert-success py-2 mb-3" style="font-size:.82rem">${this._status.ok}</div>` : nothing}
          <button class="btn btn-sm btn-primary" @click=${() => this._save(p.enabled)}>
            <i class="bi bi-check-lg me-1"></i>${t('plugins.save_config')}
          </button>`}
      </div>`;
  }

  _renderAccess() {
    return html`
      <div style="margin-top:1.75rem">
        <div class="um-header" style="padding:0 0 .5rem">
          <h3 class="um-title" style="font-size:1rem"><i class="bi bi-people me-2"></i>${t('plugins.detail.access.title')}</h3>
        </div>
        <div class="text-muted mb-2" style="font-size:.78rem">${t('plugins.access.desc')}</div>
        ${this._accessErr ? html`
          <div class="alert alert-danger py-2 mb-3" style="font-size:.82rem">${this._accessErr}</div>` : nothing}
        ${this._accessSaved ? html`
          <div class="alert alert-success py-2 mb-3" style="font-size:.82rem">${t('plugins.saved')}</div>` : nothing}
        ${this._access === null
          ? html`<div style="font-size:.8rem"><i class="bi bi-hourglass-split"></i></div>`
          : this._access.length === 0
            ? html`<div class="um-empty" style="padding:1rem"><i class="bi bi-people"></i><p>${t('plugins.access.empty')}</p></div>`
            : html`
              <div class="connector-card" style="cursor:default">
                ${this._access.map(u => html`
                  <div class="form-check">
                    <input class="form-check-input" type="checkbox" id="plugin-access-${u.user_id}"
                      .checked=${this._accessSel.has(u.user_id)}
                      @change=${(e) => this._toggleAccessUser(u.user_id, e.target.checked)} />
                    <label class="form-check-label" for="plugin-access-${u.user_id}">
                      ${u.username} <code class="text-muted" style="font-size:.7rem">${u.role_id}</code>
                    </label>
                  </div>`)}
              </div>
              <button class="btn btn-sm btn-primary mt-2" @click=${() => this._saveAccess()}>
                <i class="bi bi-check-lg me-1"></i>${t('plugins.access.save')}
              </button>`}
      </div>`;
  }
}
