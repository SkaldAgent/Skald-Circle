import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';
import { t }            from '../lib/i18n.js';
import { connectorIconUrl, statusOf, STATUS_LABEL, statusText } from './shared/connector-common.js';

// Connectors (MCP) — blueprint §7/§14/§15.
//
// **One row per connector**, not one per runtime instance. A catalog entry is a
// template with two runtimes (§7), and a person thinks in terms of "do I have
// Gmail?" — not "how many `mcp_user_servers` rows named gmail-ish do I own?". So the
// old three-section split (Mine / Global / Available) is gone: the same connector
// used to appear twice, once as a template and once as its instance, and the reader
// had to join the two by eye. Here each connector appears exactly once, and its
// state is a chip on the card.
//
// The card is a link, not a form. Everything that needs typing lives on the
// connector's own page (`#connector?name=X`) — an activation form has as many
// fields as the connector declares (EMAIL has a dozen), which a fixed-size dialog
// could never hold.
//
// Reuses the marketplace's card styling (`web/css/connectors.css`).

const ADMIN_ID = 'admin';

async function jf(url, opts) {
  const res = await fetch(url, opts);
  if (!res.ok) throw new Error(await res.text() || `HTTP ${res.status}`);
  const ct = res.headers.get('content-type') || '';
  return ct.includes('application/json') ? res.json() : null;
}

export class ConnectorsPage extends LightElement {

  static get properties() {
    return {
      _open:      { state: true },
      _me:        { state: true },   // { role_id }
      _available: { state: true },   // { catalog: [...], globals: [...] }
      _activated: { state: true },   // my per-user server rows
      _error:     { state: true },
      _q:         { state: true },
      _noIcon:    { state: true },   // names whose icon failed to load
      _providers: { state: true },   // admin: OAuth provider list (modal)
      _pForm:     { state: true },   // admin: provider being edited, or null
      _pError:    { state: true },
    };
  }

  constructor() {
    super();
    this._open = false;
    this._q = '';
    this._noIcon = new Set();
    this._reset();
  }

  _reset() {
    this._me = null;
    this._available = null;
    this._activated = null;
    this._error = null;
    this._providers = null;
    this._pForm = null;
    this._pError = null;
  }

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'connectors';
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) this._load();
    });
    window.addEventListener('connectors-changed', () => { if (this._open) this._load(); });
  }

  disconnectedCallback() {
    window.removeEventListener('locale-changed', this.__onLocaleChanged);
    super.disconnectedCallback();
  }

  get _isAdmin() { return this._me?.role_id === ADMIN_ID; }

  async _load() {
    this._error = null;
    try {
      this._me = await jf('/api/auth/me');
      const [available, activated] = await Promise.all([
        jf('/api/mcp/available'),
        jf('/api/mcp/activated'),
      ]);
      this._available = available;
      this._activated = activated;
    } catch (e) {
      this._error = e.message;
    }
  }

  _go(page, hash) {
    history.pushState({ page }, '', hash);
    window.dispatchEvent(new CustomEvent('llm-page-change', { detail: { page } }));
  }

  _openConnector(name) {
    this._go('connector', `#connector?name=${encodeURIComponent(name)}`);
  }

  // ── admin: OAuth sign-in providers (§15) ─────────────────────────────────────

  async _openProviders() {
    this._pError = null;
    this._pForm = null;
    try {
      this._providers = await jf('/api/mcp/providers');
    } catch (e) { this._pError = e.message; this._providers = []; }
  }

  _closeProviders() {
    this._providers = null;
    this._pForm = null;
    this._pError = null;
  }

  _blankProvider() {
    return { name: '', display_name: '', auth_url: '', token_url: '',
             client_id: '', client_secret: '', redirect_uri: '', extra_params: '' };
  }

  /// A Google preset — fills everything but the client_id/secret the admin pastes
  /// from their Google Cloud console. `prompt=consent` + `access_type=offline` are
  /// what make Google return a refresh token (§15).
  _presetGoogle() {
    this._pError = null;
    this._pForm = {
      name:          'google',
      display_name:  'Google',
      auth_url:      'https://accounts.google.com/o/oauth2/v2/auth',
      token_url:     'https://oauth2.googleapis.com/token',
      client_id:     '',
      client_secret: '',
      redirect_uri:  'https://connectors.skaldagent.net/oauth/show.html',
      extra_params:  '{"access_type":"offline","prompt":"consent"}',
      _isNew:        true,
    };
  }

  _editProvider(p) {
    // The secret never came back from the server; an empty box means "keep it".
    this._pForm = { ...p, client_secret: '', extra_params: p.extra_params || '', _isNew: false };
    this._pError = null;
  }

  _patchProvider(key, value) {
    this._pForm = { ...this._pForm, [key]: value };
  }

  async _saveProvider() {
    const f = this._pForm;
    if (!f.name.trim() || !f.client_id.trim()) {
      this._pError = t('connectors.providers.error.name_client');
      return;
    }
    if (f._isNew && !f.client_secret.trim()) {
      this._pError = t('connectors.providers.error.secret');
      return;
    }
    this._pError = null;
    try {
      const { _isNew, has_client_secret, ...body } = f;
      await jf('/api/mcp/providers', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      });
      this._pForm = null;
      this._providers = await jf('/api/mcp/providers');
    } catch (e) { this._pError = e.message; }
  }

  async _deleteProvider(name) {
    if (!confirm(t('connectors.providers.delete_confirm', { name }))) return;
    try {
      await jf(`/api/mcp/providers/${encodeURIComponent(name)}`, { method: 'DELETE' });
      this._providers = await jf('/api/mcp/providers');
    } catch (e) { this._pError = e.message; }
  }

  /// The merged view: every connector the caller can see, exactly once, carrying
  /// whichever runtime rows exist for it.
  get _rows() {
    const catalog   = this._available?.catalog ?? [];
    const globals   = this._available?.globals ?? [];
    const activated = this._activated ?? [];

    const rows = catalog.map(e => ({
      ...e,
      _act:  activated.find(r => r.catalog_name === e.name) ?? null,
      _glob: globals.find(g => (g.catalog_name ?? g.name) === e.name) ?? null,
    }));

    // A granted global whose catalog row the caller cannot see. `/api/mcp/available`
    // only returns `global` catalog entries to a catalog manager, so without this the
    // connector an ordinary user actually uses every day would be missing from their
    // own list — visible to the admin, invisible to its user.
    for (const g of globals) {
      const key = g.catalog_name ?? g.name;
      if (rows.some(r => r.name === key)) continue;
      rows.push({
        name:          key,
        friendly_name: g.friendly_name,
        description:   g.description,
        scope:         'global',
        source:        'remote',
        auth_kind:     'none',
        _act:          null,
        _glob:         g,
      });
    }

    const q = this._q.trim().toLowerCase();
    return rows
      .filter(r => !q
        || r.name.toLowerCase().includes(q)
        || (r.friendly_name ?? '').toLowerCase().includes(q)
        || (r.description ?? '').toLowerCase().includes(q))
      .sort((a, b) => (a.friendly_name || a.name).localeCompare(b.friendly_name || b.name));
  }

  _iconFailed(name) {
    // Re-render with the placeholder. A synthetic row (a granted global whose
    // catalog entry the caller cannot read) has no icon path to check up front, so
    // the 404 is the check.
    const next = new Set(this._noIcon);
    next.add(name);
    this._noIcon = next;
  }

  // ── Render ─────────────────────────────────────────────────────────────────

  render() {
    if (!this._open) return nothing;
    const loading = this._available === null && !this._error;
    const rows = loading ? [] : this._rows;

    return html`
      <div class="um-page">
        <div class="um-header">
          <h2 class="um-title"><i class="bi bi-plug me-2"></i>${t('connectors.title')}</h2>
          <div class="um-header-right">
            ${this._isAdmin ? html`
              <button class="btn btn-sm btn-outline-secondary" @click=${() => this._openProviders()}>
                <i class="bi bi-key me-1"></i>${t('connectors.btn.signin_providers')}
              </button>
              <button class="btn btn-sm btn-outline-secondary" @click=${() => this._go('catalog', '#catalog')}>
                <i class="bi bi-journal-text me-1"></i>${t('connectors.btn.catalog')}
              </button>
              <button class="btn btn-sm btn-primary" @click=${() => this._go('marketplace', '#marketplace')}>
                <i class="bi bi-bag me-1"></i>${t('connectors.btn.marketplace')}
              </button>` : nothing}
          </div>
        </div>

        ${this._error ? html`
          <div class="alert alert-danger py-2 mx-4" style="font-size:.85rem">${this._error}</div>` : nothing}

        ${loading
          ? html`<div class="um-empty"><i class="bi bi-hourglass-split"></i> ${t('connectors.loading')}</div>`
          : html`
            <div style="padding:0 1.25rem 1.5rem; overflow:auto">
              <div class="connector-filters">
                <div class="connector-search">
                  <i class="bi bi-search"></i>
                  <input class="form-control form-control-sm" placeholder=${t('connectors.search')}
                    .value=${this._q} @input=${(e) => { this._q = e.target.value; }} />
                </div>
              </div>
              ${rows.length === 0 ? this._renderEmpty() : html`
                <div class="connector-grid">${rows.map(r => this._renderCard(r))}</div>`}
            </div>`}
        ${this._providers !== null ? this._renderProvidersModal() : nothing}
      </div>`;
  }

  _renderProvidersModal() {
    return html`
      <div style="position:fixed;inset:0;background:rgba(0,0,0,.5);z-index:1050;
                  display:flex;align-items:flex-start;justify-content:center;overflow:auto;padding:2rem 1rem"
        @click=${(e) => { if (e.target === e.currentTarget) this._closeProviders(); }}>
        <div class="connector-card" style="width:100%;max-width:560px;cursor:default">
          <div class="d-flex align-items-center justify-content-between mb-2">
            <h3 class="um-title" style="font-size:1rem;margin:0"><i class="bi bi-key me-2"></i>${t('connectors.providers.title')}</h3>
            <button class="btn btn-sm btn-outline-secondary" @click=${() => this._closeProviders()}>
              <i class="bi bi-x-lg"></i>
            </button>
          </div>
          <div class="text-muted mb-3" style="font-size:.78rem">${t('connectors.providers.desc')}</div>
          ${this._pError ? html`
            <div class="alert alert-danger py-2 mb-2" style="font-size:.82rem">${this._pError}</div>` : nothing}
          ${this._pForm ? this._renderProviderForm() : this._renderProviderList()}
        </div>
      </div>`;
  }

  _renderProviderList() {
    const list = this._providers ?? [];
    return html`
      ${list.length === 0 ? html`
        <div class="um-empty" style="padding:1rem"><i class="bi bi-key"></i>
          <p>${t('connectors.providers.empty')}</p></div>` : html`
        <div class="d-flex flex-column gap-2 mb-3">
          ${list.map(p => html`
            <div class="d-flex align-items-center justify-content-between p-2 rounded"
                 style="border:1px solid var(--bs-border-color,#333)">
              <div style="min-width:0">
                <div style="font-weight:500">${p.display_name || p.name}
                  <code class="text-muted" style="font-size:.7rem">${p.name}</code></div>
                <div class="text-muted" style="font-size:.72rem;overflow:hidden;text-overflow:ellipsis;white-space:nowrap">
                  ${p.has_client_secret
                    ? html`<i class="bi bi-check-circle text-success"></i> ${t('connectors.providers.secret_set')}`
                    : html`<i class="bi bi-exclamation-triangle text-warning"></i> ${t('connectors.providers.no_secret')}`}
                  · ${p.client_id || t('connectors.providers.no_client_id')}
                </div>
              </div>
              <div class="d-flex gap-1">
                <button class="btn btn-sm btn-outline-secondary" @click=${() => this._editProvider(p)}>
                  <i class="bi bi-pencil"></i></button>
                <button class="btn btn-sm btn-outline-danger" @click=${() => this._deleteProvider(p.name)}>
                  <i class="bi bi-trash"></i></button>
              </div>
            </div>`)}
        </div>`}
      <div class="d-flex gap-2">
        <button class="btn btn-sm btn-primary" @click=${() => this._presetGoogle()}>
          <i class="bi bi-google me-1"></i>${t('connectors.providers.add_google')}
        </button>
        <button class="btn btn-sm btn-outline-secondary" @click=${() => { this._pForm = { ...this._blankProvider(), _isNew: true }; }}>
          <i class="bi bi-plus-lg me-1"></i>${t('connectors.providers.add_other')}
        </button>
      </div>`;
  }

  _renderProviderForm() {
    const f = this._pForm;
    const field = (key, label, opts = {}) => html`
      <div class="mb-2">
        <label class="form-label" style="font-size:.8rem">${label}${opts.req ? html`<span class="text-danger">*</span>` : nothing}</label>
        <input class="form-control form-control-sm ${opts.mono ? 'font-monospace' : ''}"
          type=${opts.secret ? 'password' : 'text'}
          placeholder=${opts.ph || ''}
          .value=${f[key] ?? ''}
          @input=${(e) => this._patchProvider(key, e.target.value)} />
        ${opts.help ? html`<div class="form-text" style="font-size:.7rem">${opts.help}</div>` : nothing}
      </div>`;
    return html`
      ${field('name', t('connectors.providers.field.name'), { req: true, mono: true, ph: 'google',
        help: t('connectors.providers.field.name_help') })}
      ${field('display_name', t('connectors.providers.field.display'), { ph: 'Google' })}
      ${field('client_id', t('connectors.providers.field.client_id'), { req: true, mono: true })}
      ${field('client_secret', t('connectors.providers.field.client_secret'), { secret: true, mono: true,
        help: f._isNew ? t('connectors.providers.field.secret_help_new') : t('connectors.providers.field.secret_help_edit') })}
      ${field('auth_url', t('connectors.providers.field.auth_url'), { mono: true, ph: 'https://accounts.google.com/o/oauth2/v2/auth' })}
      ${field('token_url', t('connectors.providers.field.token_url'), { mono: true, ph: 'https://oauth2.googleapis.com/token' })}
      ${field('redirect_uri', t('connectors.providers.field.redirect'), { mono: true,
        help: t('connectors.providers.field.redirect_help') })}
      ${field('extra_params', t('connectors.providers.field.extra'), { mono: true, ph: '{"access_type":"offline","prompt":"consent"}',
        help: t('connectors.providers.field.extra_help') })}
      <div class="d-flex gap-2 mt-3">
        <button class="btn btn-sm btn-primary" @click=${() => this._saveProvider()}>
          <i class="bi bi-check-lg me-1"></i>${t('connectors.providers.save')}
        </button>
        <button class="btn btn-sm btn-outline-secondary" @click=${() => { this._pForm = null; this._pError = null; }}>
          ${t('connectors.providers.cancel')}
        </button>
      </div>`;
  }

  _renderEmpty() {
    if (this._q.trim()) {
      return html`<div class="um-empty" style="padding:1rem"><i class="bi bi-search"></i>
        <p>${t('connectors.empty.match', { query: this._q })}</p></div>`;
    }
    return html`
      <div class="um-empty" style="padding:1rem"><i class="bi bi-plug"></i>
        <p>${this._isAdmin ? t('connectors.empty.installed') : t('connectors.empty.available')}</p>
        ${this._isAdmin
          ? html`<p style="font-size:.8rem;opacity:.7">${t('connectors.empty.install_hint')}</p>`
          : html`<p style="font-size:.8rem;opacity:.7">${t('connectors.empty.ask_admin')}</p>`}
      </div>`;
  }

  _renderCard(r) {
    const status   = statusOf(r);
    const isGlobal = r.scope === 'global';
    const isScript = r.source === 'local_script';
    const showIcon = !this._noIcon.has(r.name);

    return html`
      <div class="connector-card" role="button" tabindex="0"
        style="cursor:pointer"
        @click=${() => this._openConnector(r.name)}
        @keydown=${(e) => { if (e.key === 'Enter' || e.key === ' ') { e.preventDefault(); this._openConnector(r.name); } }}>
        <div class="connector-card-head">
          ${showIcon
            ? html`<img class="connector-card-icon" src=${connectorIconUrl(r.name, 'sm')} alt=""
                     @error=${() => this._iconFailed(r.name)} />`
            : html`<div class="connector-card-icon connector-card-icon--empty"><i class="bi bi-plug"></i></div>`}
          <div class="connector-card-title">
            <div class="connector-card-name">${r.friendly_name || r.name}</div>
            <div class="connector-card-sub">${r.name}</div>
          </div>
          <span class=${`connector-chip${STATUS_LABEL[status].tone ? ` connector-chip--${STATUS_LABEL[status].tone}` : ''}`}>
            ${statusText(status)}
          </span>
        </div>

        ${r.description ? html`<div class="connector-card-desc">${r.description}</div>` : nothing}

        <div class="connector-chips">
          <span class="connector-chip connector-chip--scope">
            <i class="bi ${isGlobal ? 'bi-globe' : 'bi-person'}"></i>${isGlobal ? t('connectors.chip.global') : t('connectors.chip.per_user')}
          </span>
          ${isScript ? html`
            <span class="connector-chip connector-chip--script">
              <i class="bi bi-file-earmark-code"></i>${t('connectors.chip.local_script')}
            </span>` : nothing}
          ${r.auth_kind && r.auth_kind !== 'none' ? html`
            <span class="connector-chip"><i class="bi bi-key"></i>${r.auth_kind}</span>` : nothing}
        </div>
      </div>`;
  }
}
