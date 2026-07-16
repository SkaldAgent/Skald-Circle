import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';

// Connectors (MCP) management — blueprint §14/§15.
//
// Two audiences on one page:
//  • every user: activate/deactivate per-user connectors from the catalog, and
//    see the global connectors they've been granted;
//  • admin (role_id === 'admin'): curate the catalog and enable globally-active
//    connectors + grant per-user access.
//
// Reuses the shared `um-*` / bootstrap styling (no page-specific CSS).

const ADMIN_ID = 'admin';

function parseJson(s, fallback) {
  if (!s) return fallback;
  try { return JSON.parse(s); } catch { return fallback; }
}

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
      _available: { state: true },   // { catalog: [...], global: [names] }
      _activated: { state: true },   // [ user server rows ]
      _catalog:   { state: true },   // admin: catalog rows
      _global:    { state: true },   // admin: global server rows
      _users:     { state: true },   // admin: user summaries (for access)
      _access:    { state: true },   // admin: { server_id -> Set(user_id) } (loaded lazily)
      _error:     { state: true },
      _modal:     { state: true },
    };
  }

  constructor() {
    super();
    this._open = false;
    this._reset();
  }

  _reset() {
    this._me = null;
    this._available = null;
    this._activated = null;
    this._catalog = null;
    this._global = null;
    this._users = null;
    this._error = null;
    this._modal = null;
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'connectors';
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) this._load();
    });
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
      if (this._isAdmin) {
        const [catalog, global, users] = await Promise.all([
          jf('/api/mcp/catalog'),
          jf('/api/mcp/global'),
          jf('/api/users'),
        ]);
        this._catalog = catalog;
        this._global = global;
        this._users = users;
      }
    } catch (e) {
      this._error = e.message;
    }
  }

  _patch(field, value) {
    this._modal = { ...this._modal, form: { ...this._modal.form, [field]: value } };
  }

  _closeModal() { this._modal = null; this._error = null; }

  // ── User: activate / deactivate ────────────────────────────────────────────

  _openActivate(entry) {
    const schema = parseJson(entry.config_schema_json, []);
    this._modal = {
      kind: 'activate',
      entry,
      form: { name: entry.name, api_key: '', env: Object.fromEntries((schema || []).map(k => [k, ''])) },
    };
  }

  async _activate() {
    const { entry, form } = this._modal;
    if (!form.name.trim()) { this._error = 'A name is required.'; return; }
    const env = {};
    for (const [k, v] of Object.entries(form.env || {})) if (v !== '') env[k] = v;
    try {
      await jf('/api/mcp/activate', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          catalog_name: entry.name,
          name: form.name.trim(),
          api_key: form.api_key || null,
          env: Object.keys(env).length ? env : null,
        }),
      });
      this._closeModal();
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  async _deactivate(row) {
    if (!confirm(`Deactivate connector "${row.name}"?`)) return;
    try {
      await jf(`/api/mcp/activated/${row.id}`, { method: 'DELETE' });
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  // ── Admin: catalog ─────────────────────────────────────────────────────────

  _openCatalogNew() {
    this._modal = {
      kind: 'catalog',
      form: {
        name: '', scope: 'per_user', source: 'remote', transport: 'stdio',
        command: '', args: '', url: '', script_path: '', config_schema: '',
        auth_kind: 'none', friendly_name: '', description: '',
      },
    };
  }

  async _saveCatalog() {
    const f = this._modal.form;
    if (!f.name.trim()) { this._error = 'Name is required.'; return; }
    const listField = (s) => s.split(/[\n,]/).map(x => x.trim()).filter(Boolean);
    try {
      await jf('/api/mcp/catalog', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          name: f.name.trim(),
          scope: f.scope,
          source: f.source,
          transport: f.transport,
          command: f.command.trim() || null,
          args: f.args.trim() ? listField(f.args) : null,
          url: f.url.trim() || null,
          script_path: f.script_path.trim() || null,
          config_schema: f.config_schema.trim() ? listField(f.config_schema) : null,
          auth_kind: f.auth_kind,
          friendly_name: f.friendly_name.trim() || null,
          description: f.description.trim() || null,
        }),
      });
      this._closeModal();
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  async _deleteCatalog(row) {
    if (!confirm(`Delete catalog entry "${row.name}"?`)) return;
    try {
      await jf(`/api/mcp/catalog/${row.id}`, { method: 'DELETE' });
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  // ── Admin: global connectors + access ──────────────────────────────────────

  _openGlobalEnable() {
    const globals = (this._catalog ?? []).filter(c => c.scope === 'global');
    this._modal = {
      kind: 'global',
      globals,
      form: { catalog_name: globals[0]?.name ?? '', name: '', api_key: '' },
    };
  }

  async _enableGlobal() {
    const f = this._modal.form;
    if (!f.catalog_name) { this._error = 'Pick a catalog entry.'; return; }
    try {
      await jf('/api/mcp/global', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          catalog_name: f.catalog_name,
          name: f.name.trim() || null,
          api_key: f.api_key || null,
        }),
      });
      this._closeModal();
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  async _deleteGlobal(row) {
    if (!confirm(`Remove global connector "${row.name}"?`)) return;
    try {
      await jf(`/api/mcp/global/${row.id}`, { method: 'DELETE' });
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  async _openAccess(server) {
    this._modal = { kind: 'access', server, selected: new Set() };
    try {
      const current = await jf(`/api/mcp/global/${server.id}/access`);
      // Ignore if the admin already navigated away / opened another modal.
      if (this._modal?.kind === 'access' && this._modal.server.id === server.id) {
        this._modal = { ...this._modal, selected: new Set(current || []) };
      }
    } catch (e) { this._error = e.message; }
  }

  _toggleAccess(userId) {
    const sel = new Set(this._modal.selected);
    sel.has(userId) ? sel.delete(userId) : sel.add(userId);
    this._modal = { ...this._modal, selected: sel };
  }

  async _saveAccess() {
    const { server, selected } = this._modal;
    try {
      await jf(`/api/mcp/global/${server.id}/access`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ user_ids: [...selected] }),
      });
      this._closeModal();
    } catch (e) { this._error = e.message; }
  }

  // ── Render ─────────────────────────────────────────────────────────────────

  render() {
    if (!this._open) return nothing;
    const loading = this._available === null && !this._error;

    return html`
      <div class="um-page">
        <div class="um-header">
          <h2 class="um-title"><i class="bi bi-plug me-2"></i>Connectors</h2>
        </div>

        ${this._error && !this._modal ? html`
          <div class="alert alert-danger py-2 mx-4" style="font-size:.85rem">${this._error}</div>
        ` : nothing}

        ${loading ? html`<div class="um-empty"><i class="bi bi-hourglass-split"></i> Loading…</div>` : html`
          <div style="padding:0 1.25rem 1.5rem; overflow:auto">
            ${this._renderMine()}
            ${this._renderAvailable()}
            ${this._isAdmin ? this._renderAdmin() : nothing}
          </div>
        `}
      </div>
      ${this._renderModal()}
    `;
  }

  _section(title, icon, right, body) {
    return html`
      <div style="margin-top:1.5rem">
        <div class="um-header" style="padding:0 0 .5rem">
          <h3 class="um-title" style="font-size:1rem"><i class="bi ${icon} me-2"></i>${title}</h3>
          <div class="um-header-right">${right ?? nothing}</div>
        </div>
        ${body}
      </div>`;
  }

  _renderMine() {
    const rows = this._activated ?? [];
    const globals = this._available?.global ?? [];
    return this._section('My connectors', 'bi-check2-circle', nothing, html`
      ${globals.length ? html`
        <div class="mb-2" style="font-size:.8rem;color:var(--text-muted,#888)">
          Global (granted by admin): ${globals.map(g => html`<code class="me-1">${g}</code>`)}
        </div>` : nothing}
      ${rows.length === 0 ? html`<div class="um-empty" style="padding:1rem"><i class="bi bi-plug"></i><p>No per-user connectors activated.</p></div>` : html`
        <table class="um-table">
          <thead><tr><th>Name</th><th>Source</th><th>From catalog</th><th></th></tr></thead>
          <tbody>
            ${rows.map(r => html`
              <tr>
                <td><strong>${r.name}</strong></td>
                <td>${r.source}</td>
                <td>${r.catalog_name ? html`<code>${r.catalog_name}</code>` : html`<span class="text-muted">—</span>`}</td>
                <td><div class="um-actions">
                  <button class="um-btn-icon" title="Deactivate" @click=${() => this._deactivate(r)}><i class="bi bi-trash"></i></button>
                </div></td>
              </tr>`)}
          </tbody>
        </table>`}
    `);
  }

  _renderAvailable() {
    const entries = this._available?.catalog ?? [];
    if (entries.length === 0) return nothing;
    const activatedNames = new Set((this._activated ?? []).map(r => r.catalog_name));
    return this._section('Available to activate', 'bi-plus-square', nothing, html`
      <table class="um-table">
        <thead><tr><th>Connector</th><th>Source</th><th>Auth</th><th></th></tr></thead>
        <tbody>
          ${entries.map(e => html`
            <tr>
              <td><strong>${e.friendly_name || e.name}</strong>
                ${e.description ? html`<div class="text-muted" style="font-size:.78rem">${e.description}</div>` : nothing}</td>
              <td>${e.source}</td>
              <td>${e.auth_kind}</td>
              <td><div class="um-actions">
                <button class="btn btn-sm btn-primary" @click=${() => this._openActivate(e)}>
                  <i class="bi bi-plug me-1"></i>Activate
                </button>
                ${activatedNames.has(e.name) ? html`<span class="badge bg-success ms-1">active</span>` : nothing}
              </div></td>
            </tr>`)}
        </tbody>
      </table>
    `);
  }

  _renderAdmin() {
    const catalog = this._catalog ?? [];
    const global = this._global ?? [];
    return html`
      <hr style="margin:1.75rem 0;opacity:.4" />
      ${this._section('Catalog', 'bi-journal-text', html`
        <button class="btn btn-sm btn-primary" @click=${() => this._openCatalogNew()}><i class="bi bi-plus-lg me-1"></i>New entry</button>
      `, catalog.length === 0 ? html`<div class="um-empty" style="padding:1rem"><i class="bi bi-journal"></i><p>Empty catalog.</p></div>` : html`
        <table class="um-table">
          <thead><tr><th>Name</th><th>Scope</th><th>Source</th><th>Transport</th><th></th></tr></thead>
          <tbody>
            ${catalog.map(c => html`
              <tr>
                <td><strong>${c.name}</strong>${c.friendly_name ? html` <span class="text-muted">(${c.friendly_name})</span>` : nothing}</td>
                <td>${c.scope}</td>
                <td>${c.source}</td>
                <td>${c.transport}</td>
                <td><div class="um-actions">
                  <button class="um-btn-icon" title="Delete" @click=${() => this._deleteCatalog(c)}><i class="bi bi-trash"></i></button>
                </div></td>
              </tr>`)}
          </tbody>
        </table>
      `)}

      ${this._section('Global connectors', 'bi-globe', html`
        <button class="btn btn-sm btn-primary" @click=${() => this._openGlobalEnable()}><i class="bi bi-plus-lg me-1"></i>Enable global</button>
      `, global.length === 0 ? html`<div class="um-empty" style="padding:1rem"><i class="bi bi-globe"></i><p>No global connectors.</p></div>` : html`
        <table class="um-table">
          <thead><tr><th>Name</th><th>Transport</th><th>Enabled</th><th></th></tr></thead>
          <tbody>
            ${global.map(g => html`
              <tr>
                <td><strong>${g.name}</strong></td>
                <td>${g.transport}</td>
                <td>${g.enabled ? html`<span class="badge bg-success">on</span>` : html`<span class="badge bg-secondary">off</span>`}</td>
                <td><div class="um-actions">
                  <button class="um-btn-icon" title="Manage access" @click=${() => this._openAccess(g)}><i class="bi bi-people"></i></button>
                  <button class="um-btn-icon" title="Remove" @click=${() => this._deleteGlobal(g)}><i class="bi bi-trash"></i></button>
                </div></td>
              </tr>`)}
          </tbody>
        </table>
      `)}
    `;
  }

  // ── Modals ─────────────────────────────────────────────────────────────────

  _modalShell(title, icon, body, onSave, saveLabel) {
    return html`
      <div class="um-modal-overlay" @click=${(e) => { if (e.target.classList.contains('um-modal-overlay')) this._closeModal(); }}>
        <div class="um-modal">
          <div class="um-modal-header">
            <i class="bi ${icon}"></i><span>${title}</span>
            <button class="um-btn-icon ms-auto" @click=${() => this._closeModal()}><i class="bi bi-x-lg"></i></button>
          </div>
          <div class="um-modal-body">
            ${this._error ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:.85rem">${this._error}</div>` : nothing}
            ${body}
          </div>
          <div class="um-modal-footer">
            <button class="btn btn-sm btn-outline-secondary" @click=${() => this._closeModal()}>Cancel</button>
            <button class="btn btn-sm btn-primary" @click=${onSave}><i class="bi bi-check-lg me-1"></i>${saveLabel}</button>
          </div>
        </div>
      </div>`;
  }

  _field(label, value, oninput, opts = {}) {
    return html`<div class="mb-3">
      <label class="form-label">${label}${opts.hint ? html` <span class="text-muted">(${opts.hint})</span>` : nothing}</label>
      <input class="form-control ${opts.mono ? 'font-monospace' : ''}" type=${opts.type || 'text'}
        placeholder=${opts.placeholder || ''} .value=${value} @input=${oninput} />
    </div>`;
  }

  _select(label, value, options, onchange) {
    return html`<div class="mb-3">
      <label class="form-label">${label}</label>
      <select class="form-select" @change=${onchange}>
        ${options.map(o => html`<option value=${o} ?selected=${value === o}>${o}</option>`)}
      </select>
    </div>`;
  }

  _renderModal() {
    if (!this._modal) return nothing;
    const m = this._modal;

    if (m.kind === 'activate') {
      const f = m.form;
      const schema = parseJson(m.entry.config_schema_json, []) || [];
      return this._modalShell(`Activate ${m.entry.friendly_name || m.entry.name}`, 'bi-plug', html`
        ${this._field('Name', f.name, e => this._patch('name', e.target.value), { hint: 'unique for you', mono: true })}
        ${m.entry.auth_kind === 'api_key' ? this._field('API key', f.api_key, e => this._patch('api_key', e.target.value), { type: 'password', mono: true }) : nothing}
        ${schema.map(k => html`<div class="mb-3">
          <label class="form-label font-monospace" style="font-size:.8rem">${k}</label>
          <input class="form-control font-monospace" .value=${f.env[k] ?? ''}
            @input=${e => this._patch('env', { ...f.env, [k]: e.target.value })} />
        </div>`)}
      `, () => this._activate(), 'Activate');
    }

    if (m.kind === 'catalog') {
      const f = m.form;
      const isScript = f.source === 'local_script';
      return this._modalShell('New catalog entry', 'bi-journal-plus', html`
        ${this._field('Name', f.name, e => this._patch('name', e.target.value), { hint: 'slug', mono: true })}
        ${this._select('Scope', f.scope, ['per_user', 'global'], e => this._patch('scope', e.target.value))}
        ${this._select('Source', f.source, ['remote', 'local_script'], e => this._patch('source', e.target.value))}
        ${this._select('Transport', f.transport, ['stdio', 'http', 'sse'], e => this._patch('transport', e.target.value))}
        ${isScript
          ? html`${this._field('Command', f.command, e => this._patch('command', e.target.value), { placeholder: 'python', mono: true })}
                 ${this._field('Script path', f.script_path, e => this._patch('script_path', e.target.value), { hint: 'under ./scripts', mono: true })}`
          : this._field('URL', f.url, e => this._patch('url', e.target.value), { mono: true })}
        ${this._field('Args', f.args, e => this._patch('args', e.target.value), { hint: 'one per line', mono: true })}
        ${this._field('Required secret/env keys', f.config_schema, e => this._patch('config_schema', e.target.value), { hint: 'comma/newline', mono: true })}
        ${this._select('Auth', f.auth_kind, ['none', 'api_key', 'oauth', 'qr', 'ssh_key'], e => this._patch('auth_kind', e.target.value))}
        ${this._field('Friendly name', f.friendly_name, e => this._patch('friendly_name', e.target.value))}
        ${this._field('Description', f.description, e => this._patch('description', e.target.value))}
      `, () => this._saveCatalog(), 'Create');
    }

    if (m.kind === 'global') {
      const f = m.form;
      return this._modalShell('Enable global connector', 'bi-globe', html`
        ${m.globals.length === 0 ? html`<div class="text-muted mb-2">No <code>global</code>-scoped catalog entries yet. Add one to the catalog first.</div>` : nothing}
        ${this._select('Catalog entry', f.catalog_name, m.globals.map(g => g.name), e => this._patch('catalog_name', e.target.value))}
        ${this._field('Name override', f.name, e => this._patch('name', e.target.value), { hint: 'optional', mono: true })}
        ${this._field('API key', f.api_key, e => this._patch('api_key', e.target.value), { type: 'password', mono: true })}
      `, () => this._enableGlobal(), 'Enable');
    }

    if (m.kind === 'access') {
      const users = this._users ?? [];
      return this._modalShell(`Access — ${m.server.name}`, 'bi-people', html`
        <div class="text-muted mb-2" style="font-size:.8rem">Select who may use this global connector. This replaces the current list.</div>
        ${users.map(u => html`<div class="form-check">
          <input class="form-check-input" type="checkbox" id=${'acc-' + u.id}
            .checked=${m.selected.has(u.id)} @change=${() => this._toggleAccess(u.id)} />
          <label class="form-check-label" for=${'acc-' + u.id}>${u.display_name || u.username} <code class="text-muted">${u.id}</code></label>
        </div>`)}
      `, () => this._saveAccess(), 'Save access');
    }

    return nothing;
  }
}
