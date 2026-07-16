import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';

// Connectors (MCP) — blueprint §7/§14/§15.
//
// One question: **what is running, and what can I add?** This is the runtime view —
// literally `UserMcpView` (global ∪ per-user) plus the actions that create those
// instances. What this box *offers* is a different question, answered by the
// Connector Catalog page.
//
// The same page serves everyone; the admin just has more verbs. A catalog entry is a
// template with two runtimes (§7), so "Available" is one list with the verb that fits
// each row: a `per_user` entry says Activate (anyone), a `global` entry says Enable
// globally (admin only). Enabling a global is the admin's counterpart to activating a
// per-user one — which is why they live side by side instead of in an admin dungeon.
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
      _available: { state: true },   // { catalog: [...], globals: [...] }
      _activated: { state: true },   // my per-user server rows
      _users:     { state: true },   // admin: user summaries (for the access modal)
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
      // Only the access modal needs the user list, and only an admin opens it.
      if (this._isAdmin) this._users = await jf('/api/users');
    } catch (e) {
      this._error = e.message;
    }
  }

  _patch(field, value) {
    this._modal = { ...this._modal, form: { ...this._modal.form, [field]: value } };
  }

  _closeModal() { this._modal = null; this._error = null; }

  _goCatalog() {
    history.pushState({ page: 'catalog' }, '', '#catalog');
    window.dispatchEvent(new CustomEvent('llm-page-change', { detail: { page: 'catalog' } }));
  }

  // ── Activate a per-user connector ──────────────────────────────────────────

  _openActivate(entry) {
    const schema = parseJson(entry.config_schema_json, []) || [];
    this._modal = {
      kind: 'activate',
      entry,
      form: { name: entry.name, api_key: '', env: Object.fromEntries(schema.map(k => [k, ''])) },
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

  // ── Enable a global connector (admin) ──────────────────────────────────────

  // The entry comes from the row the admin clicked, so there is no catalog picker:
  // the old dropdown existed only because this action lived on a page that did not
  // show the catalog.
  _openEnableGlobal(entry) {
    this._modal = {
      kind: 'global',
      entry,
      form: { name: entry.name, api_key: '' },
    };
  }

  async _enableGlobal() {
    const { entry, form } = this._modal;
    try {
      await jf('/api/mcp/global', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          catalog_name: entry.name,
          name: form.name.trim() || null,
          api_key: form.api_key || null,
        }),
      });
      this._closeModal();
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  async _deleteGlobal(row) {
    if (!confirm(`Disable global connector "${row.name}"?\n\nIt stops for everyone who can use it.`)) return;
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
      await this._load();
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
          <div class="um-header-right">
            ${this._isAdmin ? html`
              <button class="btn btn-sm btn-outline-secondary" @click=${() => this._goCatalog()}>
                <i class="bi bi-journal-text me-1"></i>Catalog
              </button>` : nothing}
          </div>
        </div>

        ${this._error && !this._modal ? html`
          <div class="alert alert-danger py-2 mx-4" style="font-size:.85rem">${this._error}</div>` : nothing}

        ${loading ? html`<div class="um-empty"><i class="bi bi-hourglass-split"></i> Loading…</div>` : html`
          <div style="padding:0 1.25rem 1.5rem; overflow:auto">
            ${this._renderMine()}
            ${this._renderGlobals()}
            ${this._renderAvailable()}
          </div>`}
      </div>
      ${this._renderModal()}`;
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
    return this._section('My connectors', 'bi-check2-circle', nothing,
      rows.length === 0
        ? html`<div class="um-empty" style="padding:1rem"><i class="bi bi-plug"></i>
                 <p>No per-user connectors activated.</p></div>`
        : html`
          <table class="um-table">
            <thead><tr><th>Name</th><th>Type</th><th>From catalog</th><th></th></tr></thead>
            <tbody>
              ${rows.map(r => html`
                <tr>
                  <td><strong>${r.name}</strong></td>
                  <td><span class="badge ${r.source === 'local_script' ? 'bg-warning text-dark' : 'bg-secondary'}"
                        style="font-size:.65rem">${r.source === 'local_script' ? 'local script' : 'remote'}</span></td>
                  <td>${r.catalog_name ? html`<code>${r.catalog_name}</code>` : html`<span class="text-muted">—</span>`}</td>
                  <td><div class="um-actions">
                    <button class="um-btn-icon" title="Deactivate" @click=${() => this._deactivate(r)}>
                      <i class="bi bi-trash"></i></button>
                  </div></td>
                </tr>`)}
            </tbody>
          </table>`);
  }

  _renderGlobals() {
    const rows = this._available?.globals ?? [];
    if (rows.length === 0 && !this._isAdmin) return nothing;
    return this._section('Global connectors', 'bi-globe', nothing,
      rows.length === 0
        ? html`<div class="um-empty" style="padding:1rem"><i class="bi bi-globe"></i>
                 <p>None enabled. Enable one from Available below.</p></div>`
        : html`
          ${this._isAdmin ? html`
            <div class="text-muted mb-2" style="font-size:.75rem">
              Shared by the household. You see every one so you can manage it —
              <span class="badge bg-success" style="font-size:.6rem">yours</span> marks the ones granted to you.
            </div>` : nothing}
          <table class="um-table">
            <thead><tr><th>Name</th><th>Transport</th><th>Status</th><th></th></tr></thead>
            <tbody>
              ${rows.map(g => html`
                <tr>
                  <td>
                    <strong>${g.friendly_name || g.name}</strong>
                    ${this._isAdmin && g.can_use ? html`
                      <span class="badge bg-success ms-1" style="font-size:.6rem">yours</span>` : nothing}
                    ${g.description ? html`
                      <div class="text-muted" style="font-size:.75rem;max-width:44ch;overflow:hidden;
                                  text-overflow:ellipsis;white-space:nowrap" title=${g.description}>${g.description}</div>` : nothing}
                  </td>
                  <td><span class="text-muted" style="font-size:.78rem">${g.transport}</span></td>
                  <td>${g.enabled
                    ? html`<span class="badge bg-success" style="font-size:.65rem">on</span>`
                    : html`<span class="badge bg-secondary" style="font-size:.65rem">off</span>`}</td>
                  <td><div class="um-actions">
                    ${this._isAdmin ? html`
                      <button class="um-btn-icon" title="Manage access" @click=${() => this._openAccess(g)}>
                        <i class="bi bi-people"></i></button>
                      <button class="um-btn-icon" title="Disable" @click=${() => this._deleteGlobal(g)}>
                        <i class="bi bi-trash"></i></button>
                    ` : nothing}
                  </div></td>
                </tr>`)}
            </tbody>
          </table>`);
  }

  _renderAvailable() {
    const entries = this._available?.catalog ?? [];
    const enabledGlobals = new Set((this._available?.globals ?? []).map(g => g.catalog_name ?? g.name));
    const activatedNames = new Set((this._activated ?? []).map(r => r.catalog_name));

    const right = this._isAdmin ? html`
      <button class="btn btn-sm btn-outline-secondary" @click=${() => this._goCatalog()}>
        <i class="bi bi-plus-lg me-1"></i>Add to catalog
      </button>` : nothing;

    if (entries.length === 0) {
      return this._section('Available', 'bi-plus-square', right, html`
        <div class="um-empty" style="padding:1rem"><i class="bi bi-journal"></i>
          <p>${this._isAdmin ? 'The catalog is empty.' : 'Nothing available to you yet.'}</p>
          ${this._isAdmin ? html`
            <p style="font-size:.8rem;opacity:.7">Add connectors to the catalog first.</p>` : nothing}
        </div>`);
    }

    return this._section('Available', 'bi-plus-square', right, html`
      <table class="um-table">
        <thead><tr><th>Connector</th><th>Scope</th><th>Auth</th><th></th></tr></thead>
        <tbody>
          ${entries.map(e => {
            const isGlobal = e.scope === 'global';
            const already = isGlobal ? enabledGlobals.has(e.name) : activatedNames.has(e.name);
            return html`
              <tr>
                <td><strong>${e.friendly_name || e.name}</strong>
                  ${e.description ? html`
                    <div class="text-muted" style="font-size:.75rem;max-width:44ch;overflow:hidden;
                                text-overflow:ellipsis;white-space:nowrap" title=${e.description}>${e.description}</div>` : nothing}</td>
                <td><span class="badge ${isGlobal ? 'bg-info' : 'bg-secondary'}" style="font-size:.65rem">
                  ${isGlobal ? 'global' : 'per-user'}</span></td>
                <td><span class="text-muted" style="font-size:.78rem">${e.auth_kind}</span></td>
                <td><div class="um-actions">
                  ${already
                    ? html`<span class="badge bg-success">${isGlobal ? 'enabled' : 'active'}</span>`
                    : isGlobal
                      ? html`<button class="btn btn-sm btn-primary" @click=${() => this._openEnableGlobal(e)}>
                               <i class="bi bi-globe me-1"></i>Enable globally</button>`
                      : html`<button class="btn btn-sm btn-primary" @click=${() => this._openActivate(e)}>
                               <i class="bi bi-plug me-1"></i>Activate</button>`}
                </div></td>
              </tr>`;
          })}
        </tbody>
      </table>`);
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

  _renderModal() {
    if (!this._modal) return nothing;
    const m = this._modal;

    if (m.kind === 'activate') {
      const f = m.form;
      const schema = parseJson(m.entry.config_schema_json, []) || [];
      return this._modalShell(`Activate ${m.entry.friendly_name || m.entry.name}`, 'bi-plug', html`
        ${this._field('Name', f.name, e => this._patch('name', e.target.value), { hint: 'unique for you', mono: true })}
        ${m.entry.auth_kind === 'api_key' ? this._field('API key', f.api_key, e => this._patch('api_key', e.target.value), { type: 'password', mono: true }) : nothing}
        ${m.entry.auth_kind === 'oauth' ? html`
          <div class="alert alert-warning py-2" style="font-size:.78rem">
            <i class="bi bi-exclamation-triangle me-1"></i>This connector needs an interactive login,
            which is not wired up yet — it will activate but cannot authenticate.
          </div>` : nothing}
        ${schema.map(k => html`<div class="mb-3">
          <label class="form-label font-monospace" style="font-size:.8rem">${k}</label>
          <input class="form-control font-monospace" .value=${f.env[k] ?? ''}
            @input=${e => this._patch('env', { ...f.env, [k]: e.target.value })} />
        </div>`)}
      `, () => this._activate(), 'Activate');
    }

    if (m.kind === 'global') {
      const f = m.form;
      return this._modalShell(`Enable ${m.entry.friendly_name || m.entry.name} globally`, 'bi-globe', html`
        <div class="text-muted mb-3" style="font-size:.78rem">
          Runs once for the household on the host. Nobody reaches it until you grant access.
        </div>
        ${this._field('Name', f.name, e => this._patch('name', e.target.value), { hint: 'runtime name', mono: true })}
        ${m.entry.auth_kind === 'api_key'
          ? this._field('API key', f.api_key, e => this._patch('api_key', e.target.value), { type: 'password', mono: true })
          : nothing}
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
