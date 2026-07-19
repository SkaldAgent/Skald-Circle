import { html, nothing } from 'lit';
import { unsafeHTML }     from 'lit/directives/unsafe-html.js';
import { LightElement } from '../lib/base.js';
import { t }            from '../lib/i18n.js';

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
// The manual path is a dedicated page (`#catalog/new`), not a dialog: the form is
// long and technical, a fixed modal grew taller than the viewport with no way to
// scroll, and a click on the overlay discarded everything typed so far. A page
// scrolls, and leaving it is a deliberate navigation.
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
      _view:    { state: true },   // 'list' | 'new'
      _form:    { state: true },   // manual-entry fields, when _view === 'new'
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
    this._view = 'list';
    this._form = null;
  }

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'catalog';
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) { this._syncViewFromHash(); this._load(); }
    });
    window.addEventListener('hashchange', () => {
      if (this._open) this._syncViewFromHash();
    });
    document.addEventListener('click', () => { if (this._addOpen) this._addOpen = false; });
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

  // The `new` view is derived from the `#catalog/new` sub-route, so the browser's
  // Back/Forward works and a pasted URL lands on the form. Entering the view
  // always starts a fresh form.
  _syncViewFromHash() {
    const parts = location.hash.slice(1).split('/');
    const wantsNew = parts[0] === 'catalog' && parts[1] === 'new';
    if (wantsNew && this._view !== 'new') {
      this._error = null;
      this._form = {
        name: '', scope: 'per_user', source: 'remote', transport: 'stdio',
        command: '', args: '', url: '', script_path: '', config_schema: '',
        auth_kind: 'none', friendly_name: '', description: '',
      };
    }
    this._view = wantsNew ? 'new' : 'list';
  }

  _openManual() {
    this._addOpen = false;
    history.pushState({ page: 'catalog', view: 'new' }, '', '#catalog/new');
    this._syncViewFromHash();
  }

  _closeNew() {
    // Prefer real history so the browser's own Back stays consistent; fall back to
    // the list when this page was opened straight from a pasted URL.
    if (history.length > 1) { history.back(); return; }
    history.pushState({ page: 'catalog' }, '', '#catalog');
    this._view = 'list';
  }

  _patch(field, value) {
    this._form = { ...this._form, [field]: value };
  }

  async _saveManual() {
    const f = this._form;
    if (!f.name.trim()) { this._error = t('catalog.error.name'); return; }
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
      this._view = 'list';
      this._form = null;
      history.pushState({ page: 'catalog' }, '', '#catalog');
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  async _delete(row) {
    if (!confirm(t('catalog.confirm.delete', { name: row.name }))) return;
    try {
      await jf(`/api/mcp/catalog/${row.id}`, { method: 'DELETE' });
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  // ── Render ─────────────────────────────────────────────────────────────────

  render() {
    if (!this._open) return nothing;
    if (this._view === 'new') return this._renderNew();
    const rows = this._rows ?? [];
    const loading = this._rows === null && !this._error && this._isAdmin;

    return html`
      <div class="um-page">
        <div class="um-header">
          <h2 class="um-title"><i class="bi bi-journal-text me-2"></i>${t('catalog.title')}</h2>
          <div class="um-header-right">
            ${this._isAdmin ? this._renderAddButton() : nothing}
          </div>
        </div>

        ${this._error ? html`
          <div class="alert alert-danger py-2 mx-4" style="font-size:.85rem">${this._error}</div>` : nothing}

        <div style="padding:0 1.25rem 1.5rem; overflow:auto">
          ${this._me && !this._isAdmin ? html`
            <div class="um-empty" style="padding:2rem">
              <i class="bi bi-shield-lock"></i>
              <p>${t('catalog.not_admin')}</p>
              <p style="font-size:.8rem;opacity:.7">${unsafeHTML(t('catalog.not_admin_link'))}</p>
            </div>
          ` : loading ? html`
            <div class="um-empty" style="padding:1rem"><i class="bi bi-hourglass-split"></i><p>${t('catalog.loading')}</p></div>
          ` : html`
            <div class="text-muted mt-3 mb-3" style="font-size:.8rem">${unsafeHTML(t('catalog.desc'))}</div>
            ${rows.length === 0 ? this._renderEmpty() : this._renderTable(rows)}
          `}
        </div>
      </div>`;
  }

  // Bootstrap's own dropdown classes, not a hand-rolled panel: 5.3 themes
  // `.dropdown-menu`/`.dropdown-item` from `data-bs-theme`, so this follows the
  // light/dark switch for free. `.show` opens it — the state is ours, not
  // Bootstrap's JS.
  _renderAddButton() {
    return html`
      <div class="dropdown" style="position:relative" @click=${(e) => e.stopPropagation()}>
        <button class="btn btn-sm btn-primary" @click=${() => { this._addOpen = !this._addOpen; }}>
          <i class="bi bi-plus-lg me-1"></i>${t('catalog.btn.add')}
          <i class="bi bi-chevron-down ms-1" style="font-size:.7rem"></i>
        </button>
        ${this._addOpen ? html`
          <div class="dropdown-menu show" style="right:0;left:auto;top:calc(100% + .25rem);min-width:280px">
            <button class="dropdown-item" style="white-space:normal" @click=${() => this._goMarketplace()}>
              <div style="display:flex;align-items:center;gap:.5rem">
                <i class="bi bi-shop"></i><strong style="font-size:.85rem">${t('catalog.dropdown.marketplace')}</strong>
              </div>
              <div class="text-muted" style="font-size:.7rem;margin-top:.15rem">${t('catalog.dropdown.marketplace_desc')}</div>
            </button>
            <div class="dropdown-divider"></div>
            <button class="dropdown-item" style="white-space:normal" @click=${() => this._openManual()}>
              <div style="display:flex;align-items:center;gap:.5rem">
                <i class="bi bi-pencil"></i><strong style="font-size:.85rem">${t('catalog.dropdown.manual')}</strong>
              </div>
              <div class="text-muted" style="font-size:.7rem;margin-top:.15rem">${t('catalog.dropdown.manual_desc')}</div>
            </button>
          </div>` : nothing}
      </div>`;
  }

  _renderEmpty() {
    return html`
      <div class="um-empty" style="padding:2rem">
        <i class="bi bi-journal"></i>
        <p>${t('catalog.empty.title')}</p>
        <p style="font-size:.8rem;opacity:.7">${t('catalog.empty.hint')}</p>
        <button class="btn btn-sm btn-primary mt-2" @click=${() => this._goMarketplace()}>
          <i class="bi bi-shop me-1"></i>${t('catalog.empty.action')}
        </button>
      </div>`;
  }

  _renderTable(rows) {
    return html`
      <table class="um-table">
        <thead><tr><th>${t('catalog.table.connector')}</th><th>${t('catalog.table.scope')}</th><th>${t('catalog.table.type')}</th><th>${t('catalog.table.auth')}</th><th></th></tr></thead>
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
                ${r.scope === 'global' ? t('catalog.badge.global') : t('catalog.badge.per_user')}</span></td>
              <td><span class="badge ${r.source === 'local_script' ? 'bg-warning text-dark' : 'bg-secondary'}" style="font-size:.65rem">
                ${r.source === 'local_script' ? t('catalog.badge.local_script') : t('catalog.badge.remote')}</span></td>
              <td><span class="text-muted" style="font-size:.78rem">${r.auth_kind}</span></td>
              <td><div class="um-actions">
                <button class="um-btn-icon" title=${t('catalog.action.remove')} @click=${() => this._delete(r)}>
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

  _area(label, value, oninput, opts = {}) {
    return html`<div class="mb-3">
      <label class="form-label">${label}${opts.hint ? html` <span class="text-muted">(${opts.hint})</span>` : nothing}</label>
      <textarea class="form-control ${opts.mono ? 'font-monospace' : ''}" rows=${opts.rows || 3}
        .value=${value} @input=${oninput}></textarea>
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

  _renderNew() {
    const f = this._form;
    const isScript = f.source === 'local_script';
    return html`
      <div class="um-page">
        <div class="um-header">
          <div class="d-flex align-items-center gap-2" style="min-width:0">
            <button class="btn btn-sm btn-outline-secondary" title=${t('catalog.new.back')} @click=${() => this._closeNew()}>
              <i class="bi bi-arrow-left"></i>
            </button>
            <h2 class="um-title" style="min-width:0;overflow:hidden;text-overflow:ellipsis">
              <i class="bi bi-pencil me-2"></i>${t('catalog.new.title')}</h2>
          </div>
        </div>
        <div style="padding:0 1.25rem 2rem; overflow:auto">
          <div style="max-width:620px">
            ${this._error ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:.85rem">${this._error}</div>` : nothing}
            ${isScript ? html`
              <div class="alert alert-warning py-2 mb-3" style="font-size:.78rem">${unsafeHTML(t('catalog.new.script_warn'))}</div>` : nothing}
            ${this._field(t('catalog.new.name'), f.name, e => this._patch('name', e.target.value), { hint: t('catalog.new.name_hint'), mono: true })}
            ${this._select(t('catalog.new.scope'), f.scope, ['per_user', 'global'], e => this._patch('scope', e.target.value))}
            ${this._select(t('catalog.new.type'), f.source, ['remote', 'local_script'], e => this._patch('source', e.target.value))}
            ${this._select(t('catalog.new.transport'), f.transport, ['stdio', 'http', 'sse'], e => this._patch('transport', e.target.value))}
            ${isScript
              ? html`${this._field(t('catalog.new.command'), f.command, e => this._patch('command', e.target.value), { placeholder: t('catalog.new.command_ph'), mono: true })}
                     ${this._field(t('catalog.new.script_path'), f.script_path, e => this._patch('script_path', e.target.value), { hint: t('catalog.new.script_path_hint'), mono: true })}`
              : this._field(t('catalog.new.url'), f.url, e => this._patch('url', e.target.value), { mono: true })}
            ${this._area(t('catalog.new.args'), f.args, e => this._patch('args', e.target.value), { hint: t('catalog.new.args_hint'), mono: true })}
            ${this._area(t('catalog.new.config_schema'), f.config_schema, e => this._patch('config_schema', e.target.value), { hint: t('catalog.new.config_schema_hint'), mono: true })}
            ${this._select(t('catalog.new.auth'), f.auth_kind, ['none', 'api_key', 'oauth', 'qr', 'ssh_key'], e => this._patch('auth_kind', e.target.value))}
            ${this._field(t('catalog.new.friendly'), f.friendly_name, e => this._patch('friendly_name', e.target.value))}
            ${this._area(t('catalog.new.desc'), f.description, e => this._patch('description', e.target.value), { hint: t('catalog.new.desc_hint'), rows: 2 })}
            <div class="d-flex justify-content-end gap-2 mt-3">
              <button class="btn btn-sm btn-outline-secondary" @click=${() => this._closeNew()}>${t('catalog.new.cancel')}</button>
              <button class="btn btn-sm btn-primary" @click=${() => this._saveManual()}>
                <i class="bi bi-check-lg me-1"></i>${t('catalog.new.save')}</button>
            </div>
          </div>
        </div>
      </div>`;
  }
}
