import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';

// Connector catalog — blueprint §14/§15. Admin only.
//
// One question: **what does this box offer?** The catalog is the shelf; nothing here
// is running. A `global` entry still needs the admin to enable it and a `per_user`
// one still needs each user to activate it — both of which happen on the Connectors
// page, where the runtime lives.
//
// Adding is one intent with two sources, so it is one button with two options rather
// than two distant affordances. Their order mirrors the trust model (§14): the
// marketplace path is vetted and hash-verified, the manual path is the escape hatch
// that puts unvetted code on the box — which is why it needs `mcp.register_local_script`
// and why it sits second.
//
// Reuses the shared `um-*` / bootstrap styling (no page-specific CSS).

const ADMIN_ID = 'admin';

async function jf(url, opts) {
  const res = await fetch(url, opts);
  if (!res.ok) throw new Error(await res.text() || `HTTP ${res.status}`);
  const ct = res.headers.get('content-type') || '';
  return ct.includes('application/json') ? res.json() : null;
}

export class CatalogPage extends LightElement {

  static get properties() {
    return {
      _open:    { state: true },
      _me:      { state: true },
      _rows:    { state: true },
      _addOpen: { state: true },   // the "Add connector" chooser
      _error:   { state: true },
      _modal:   { state: true },
    };
  }

  constructor() {
    super();
    this._open = false;
    this._reset();
  }

  _reset() {
    this._me = null;
    this._rows = null;
    this._addOpen = false;
    this._error = null;
    this._modal = null;
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'catalog';
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) this._load();
    });
    // Close the chooser when clicking anywhere else.
    document.addEventListener('click', () => { if (this._addOpen) this._addOpen = false; });
  }

  get _isAdmin() { return this._me?.role_id === ADMIN_ID; }

  async _load() {
    this._error = null;
    try {
      this._me = await jf('/api/auth/me');
      if (!this._isAdmin) return;
      this._rows = await jf('/api/mcp/catalog');
    } catch (e) {
      this._error = e.message;
    }
  }

  _goMarketplace() {
    this._addOpen = false;
    history.pushState({ page: 'marketplace' }, '', '#marketplace');
    window.dispatchEvent(new CustomEvent('llm-page-change', { detail: { page: 'marketplace' } }));
  }

  _goConnectors() {
    history.pushState({ page: 'connectors' }, '', '#connectors');
    window.dispatchEvent(new CustomEvent('llm-page-change', { detail: { page: 'connectors' } }));
  }

  // ── Manual entry ───────────────────────────────────────────────────────────

  _openManual() {
    this._addOpen = false;
    this._modal = {
      form: {
        name: '', scope: 'per_user', source: 'remote', transport: 'stdio',
        command: '', args: '', url: '', script_path: '', config_schema: '',
        auth_kind: 'none', friendly_name: '', description: '',
      },
    };
  }

  _patch(field, value) {
    this._modal = { ...this._modal, form: { ...this._modal.form, [field]: value } };
  }

  _closeModal() { this._modal = null; this._error = null; }

  async _saveManual() {
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

  async _delete(row) {
    if (!confirm(`Remove "${row.name}" from the catalog?\n\nAnything already activated from it keeps running.`)) return;
    try {
      await jf(`/api/mcp/catalog/${row.id}`, { method: 'DELETE' });
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  // ── Render ─────────────────────────────────────────────────────────────────

  render() {
    if (!this._open) return nothing;
    const rows = this._rows ?? [];
    const loading = this._rows === null && !this._error && this._isAdmin;

    return html`
      <div class="um-page">
        <div class="um-header">
          <h2 class="um-title"><i class="bi bi-journal-text me-2"></i>Connector Catalog</h2>
          <div class="um-header-right">
            ${this._isAdmin ? this._renderAddButton() : nothing}
          </div>
        </div>

        ${this._error && !this._modal ? html`
          <div class="alert alert-danger py-2 mx-4" style="font-size:.85rem">${this._error}</div>` : nothing}

        <div style="padding:0 1.25rem 1.5rem; overflow:auto">
          ${this._me && !this._isAdmin ? html`
            <div class="um-empty" style="padding:2rem">
              <i class="bi bi-shield-lock"></i>
              <p>The catalog is managed by the admin.</p>
              <p style="font-size:.8rem;opacity:.7">
                What you can activate is on the
                <a href="#connectors" @click=${(e) => { e.preventDefault(); this._goConnectors(); }}>Connectors</a> page.
              </p>
            </div>
          ` : loading ? html`
            <div class="um-empty" style="padding:1rem"><i class="bi bi-hourglass-split"></i><p>Loading…</p></div>
          ` : html`
            <div class="text-muted mt-3 mb-3" style="font-size:.8rem">
              What this box offers. Nothing here is running — a global entry still needs
              enabling, a per-user one still needs each user to activate it, both on the
              <a href="#connectors" @click=${(e) => { e.preventDefault(); this._goConnectors(); }}>Connectors</a> page.
            </div>
            ${rows.length === 0 ? this._renderEmpty() : this._renderTable(rows)}
          `}
        </div>
      </div>
      ${this._renderModal()}`;
  }

  // Bootstrap's own dropdown classes, not a hand-rolled panel: 5.3 themes
  // `.dropdown-menu`/`.dropdown-item` from `data-bs-theme`, so this follows the
  // light/dark switch for free. `.show` opens it — the state is ours, not
  // Bootstrap's JS.
  _renderAddButton() {
    return html`
      <div class="dropdown" style="position:relative" @click=${(e) => e.stopPropagation()}>
        <button class="btn btn-sm btn-primary" @click=${() => { this._addOpen = !this._addOpen; }}>
          <i class="bi bi-plus-lg me-1"></i>Add connector
          <i class="bi bi-chevron-down ms-1" style="font-size:.7rem"></i>
        </button>
        ${this._addOpen ? html`
          <div class="dropdown-menu show" style="right:0;left:auto;top:calc(100% + .25rem);min-width:280px">
            <button class="dropdown-item" style="white-space:normal" @click=${() => this._goMarketplace()}>
              <div style="display:flex;align-items:center;gap:.5rem">
                <i class="bi bi-shop"></i><strong style="font-size:.85rem">From the marketplace</strong>
              </div>
              <div class="text-muted" style="font-size:.7rem;margin-top:.15rem">
                Vetted connectors, files verified by SHA-256.
              </div>
            </button>
            <div class="dropdown-divider"></div>
            <button class="dropdown-item" style="white-space:normal" @click=${() => this._openManual()}>
              <div style="display:flex;align-items:center;gap:.5rem">
                <i class="bi bi-pencil"></i><strong style="font-size:.85rem">Manually</strong>
              </div>
              <div class="text-muted" style="font-size:.7rem;margin-top:.15rem">
                You supply the config, and vouch for it yourself.
              </div>
            </button>
          </div>` : nothing}
      </div>`;
  }

  _renderEmpty() {
    return html`
      <div class="um-empty" style="padding:2rem">
        <i class="bi bi-journal"></i>
        <p>The catalog is empty.</p>
        <p style="font-size:.8rem;opacity:.7">Add a connector from the marketplace to get started.</p>
        <button class="btn btn-sm btn-primary mt-2" @click=${() => this._goMarketplace()}>
          <i class="bi bi-shop me-1"></i>Browse the marketplace
        </button>
      </div>`;
  }

  _renderTable(rows) {
    return html`
      <table class="um-table">
        <thead><tr><th>Connector</th><th>Scope</th><th>Type</th><th>Auth</th><th></th></tr></thead>
        <tbody>
          ${rows.map(r => html`
            <tr>
              <td>
                <strong>${r.friendly_name || r.name}</strong>
                ${r.friendly_name ? html` <code class="text-muted" style="font-size:.7rem">${r.name}</code>` : nothing}
                ${r.description ? html`
                  <div class="text-muted" style="font-size:.75rem;max-width:44ch;overflow:hidden;
                              text-overflow:ellipsis;white-space:nowrap" title=${r.description}>${r.description}</div>` : nothing}
              </td>
              <td><span class="badge ${r.scope === 'global' ? 'bg-info' : 'bg-secondary'}" style="font-size:.65rem">
                ${r.scope === 'global' ? 'global' : 'per-user'}</span></td>
              <td><span class="badge ${r.source === 'local_script' ? 'bg-warning text-dark' : 'bg-secondary'}" style="font-size:.65rem">
                ${r.source === 'local_script' ? 'local script' : 'remote'}</span></td>
              <td><span class="text-muted" style="font-size:.78rem">${r.auth_kind}</span></td>
              <td><div class="um-actions">
                <button class="um-btn-icon" title="Remove from catalog" @click=${() => this._delete(r)}>
                  <i class="bi bi-trash"></i></button>
              </div></td>
            </tr>`)}
        </tbody>
      </table>`;
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
    const f = this._modal.form;
    const isScript = f.source === 'local_script';
    return html`
      <div class="um-modal-overlay" @click=${(e) => { if (e.target.classList.contains('um-modal-overlay')) this._closeModal(); }}>
        <div class="um-modal">
          <div class="um-modal-header">
            <i class="bi bi-pencil"></i><span>Add connector manually</span>
            <button class="um-btn-icon ms-auto" @click=${() => this._closeModal()}><i class="bi bi-x-lg"></i></button>
          </div>
          <div class="um-modal-body">
            ${this._error ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:.85rem">${this._error}</div>` : nothing}
            ${isScript ? html`
              <div class="alert alert-warning py-2 mb-3" style="font-size:.78rem">
                <i class="bi bi-exclamation-triangle me-1"></i>A local script runs code on this box.
                Nothing verifies it — unlike the marketplace path, there is no digest to check.
              </div>` : nothing}
            ${this._field('Name', f.name, e => this._patch('name', e.target.value), { hint: 'slug', mono: true })}
            ${this._select('Scope', f.scope, ['per_user', 'global'], e => this._patch('scope', e.target.value))}
            ${this._select('Type', f.source, ['remote', 'local_script'], e => this._patch('source', e.target.value))}
            ${this._select('Transport', f.transport, ['stdio', 'http', 'sse'], e => this._patch('transport', e.target.value))}
            ${isScript
              ? html`${this._field('Command', f.command, e => this._patch('command', e.target.value), { placeholder: 'python3', mono: true })}
                     ${this._field('Script path', f.script_path, e => this._patch('script_path', e.target.value), { hint: 'as <connector>/<file>, under ./connectors', mono: true })}`
              : this._field('URL', f.url, e => this._patch('url', e.target.value), { mono: true })}
            ${this._field('Args', f.args, e => this._patch('args', e.target.value), { hint: 'one per line', mono: true })}
            ${this._field('Required secret/env keys', f.config_schema, e => this._patch('config_schema', e.target.value), { hint: 'comma/newline', mono: true })}
            ${this._select('Auth', f.auth_kind, ['none', 'api_key', 'oauth', 'qr', 'ssh_key'], e => this._patch('auth_kind', e.target.value))}
            ${this._field('Friendly name', f.friendly_name, e => this._patch('friendly_name', e.target.value))}
            ${this._field('Description', f.description, e => this._patch('description', e.target.value),
                { hint: 'the LLM reads this when deciding to activate the connector' })}
          </div>
          <div class="um-modal-footer">
            <button class="btn btn-sm btn-outline-secondary" @click=${() => this._closeModal()}>Cancel</button>
            <button class="btn btn-sm btn-primary" @click=${() => this._saveManual()}>
              <i class="bi bi-check-lg me-1"></i>Add to catalog</button>
          </div>
        </div>
      </div>`;
  }
}
