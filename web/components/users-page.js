import { html, nothing }          from 'lit';
import { unsafeHTML }             from 'lit/directives/unsafe-html.js';
import { LightElement }           from '../lib/base.js';
import { t }                      from '../lib/i18n.js';
import { connectorIconUrl }       from './shared/connector-common.js';

// Users admin — the list at `#users`, one user's page at `#users/{id}`.
//
// The per-user surface used to be four modals (create / edit / password /
// connectors). The connectors one collapsed first: a checkbox list taller than the
// viewport with no scroll. Same failure the connector activation and manual-add
// dialogs had, same fix — a page scrolls, and leaving it is a deliberate
// navigation. Edit and password followed, so everything about one user lives in
// one place: Profile, Connectors, Plugins, Security. Only **create** stays a modal
// — it is three fields and a role, it fits.
//
// Both grant sections answer the same question — *what may this person use* — so
// they read the same way: the Connectors page's row list (icon, name, description),
// a chip for anything not enabled instance-wide, and a save that replaces the whole
// grant set. Plugins moved here from the plugin's own page for that reason: granted
// plugin-by-plugin, "what does this person have?" meant opening every plugin in
// turn, and the answer lived on N pages instead of one.

// Stable per-user avatar color: same user, same hue, everywhere (same hash as the
// topbar avatar — duplicated, it is three lines and the topbar does not export it).
function avatarColor(name) {
  let h = 0;
  for (let i = 0; i < name.length; i++) h = (h * 31 + name.charCodeAt(i)) >>> 0;
  return `hsl(${h % 360}, 55%, 52%)`;
}

export class UsersPage extends LightElement {

  static get properties() {
    return {
      _open:      { state: true },
      _users:     { state: true },
      _roles:     { state: true },
      _error:     { state: true },
      _modal:     { state: true },  // null | { mode: 'create', form }
      _view:      { state: true },  // 'list' | 'user'
      _userId:    { state: true },
      _dForm:     { state: true },  // profile form for the open user
      _dPw:       { state: true },  // security: new-password field
      _conns:     { state: true },  // working copy of the user's connector grants
      _connQ:     { state: true },
      _noIcon:    { state: true },  // connector names whose icon failed to load
      _plugs:     { state: true },  // working copy of the user's plugin grants
      _triage:    { state: true },  // { interval_minutes, default_interval_minutes } | null
      _triageIn:  { state: true },  // the input's own string ('' = follow the instance default)
      _busy:      { state: true },
      _dSaved:    { state: true },  // "saved" ticks, one per section
      _pwSaved:   { state: true },
      _connSaved: { state: true },
      _plugSaved: { state: true },
      _trgSaved:  { state: true },
    };
  }

  constructor() {
    super();
    this._open  = false;
    this._users = null;
    this._roles = null;
    this._error = null;
    this._modal = null;
    this._noIcon = new Set();
    this._resetDetail();
  }

  _resetDetail() {
    this._view   = 'list';
    this._userId = null;
    this._dForm  = null;
    this._dPw    = '';
    this._conns  = null;
    this._connQ  = '';
    this._plugs  = null;
    this._triage   = null;
    this._triageIn = '';
    this._busy      = false;
    this._dSaved    = false;
    this._pwSaved   = false;
    this._connSaved = false;
    this._plugSaved = false;
    this._trgSaved  = false;
  }

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'users';
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) { this._syncViewFromHash(); this._load(); }
    });
    window.addEventListener('hashchange', () => {
      if (this._open) this._syncViewFromHash();
    });
  }

  disconnectedCallback() {
    window.removeEventListener('locale-changed', this.__onLocaleChanged);
    super.disconnectedCallback();
  }

  async _load() {
    this._error = null;
    try {
      const [uRes, rRes] = await Promise.all([
        fetch('/api/users'),
        fetch('/api/roles'),
      ]);
      if (!uRes.ok) throw new Error(`Users: HTTP ${uRes.status}`);
      if (!rRes.ok) throw new Error(`Roles: HTTP ${rRes.status}`);
      this._users = await uRes.json();
      this._roles = await rRes.json();
      if (this._view === 'user') this._enterDetail();
    } catch (e) {
      this._error = e.message;
    }
  }

  // ── Routing: `#users` list vs `#users/{id}` detail ──────────────────────────

  _syncViewFromHash() {
    const parts = location.hash.slice(1).split('/');
    const id = parts[0] === 'users' && parts[1] ? decodeURIComponent(parts[1]) : null;
    if (!id) {
      if (this._view !== 'list') this._resetDetail();
      return;
    }
    if (id !== this._userId || this._view !== 'user') {
      this._resetDetail();
      this._view = 'user';
      this._userId = id;
      if (this._users) this._enterDetail();
    }
  }

  get _user() { return (this._users ?? []).find(u => u.id === this._userId) ?? null; }

  async _enterDetail() {
    const u = this._user;
    if (!u) { this._error = t('users.detail.no_such'); return; }
    this._dForm = {
      username: u.username, display_name: u.display_name ?? '', role_id: u.role_id,
      active: u.active, birthdate: u.birthdate ?? '', sex: u.sex ?? '', notes: u.notes ?? '',
    };
    this._dPw = '';
    try {
      const [cRes, pRes] = await Promise.all([
        fetch(`/api/users/${encodeURIComponent(u.id)}/connectors`),
        fetch(`/api/users/${encodeURIComponent(u.id)}/plugins`),
      ]);
      if (!cRes.ok) throw new Error(await cRes.text());
      if (!pRes.ok) throw new Error(await pRes.text());
      this._conns = await cRes.json();
      this._plugs = await pRes.json();
    } catch (e) { this._error = e.message; }

    // Admin-only, unlike the two above (which a role holding `plugin.manage` can
    // also reach). A refusal hides the section rather than reddening the page:
    // there is nothing wrong, this reader simply has no business with schedules.
    try {
      const res = await fetch(`/api/users/${encodeURIComponent(u.id)}/event-triage`);
      if (res.ok) {
        this._triage   = await res.json();
        this._triageIn = this._triage.interval_minutes == null ? '' : String(this._triage.interval_minutes);
      }
    } catch { /* section stays hidden */ }
  }

  _openUser(u) {
    history.pushState({ page: 'users', user: u.id }, '', `#users/${encodeURIComponent(u.id)}`);
    this._syncViewFromHash();
  }

  _back() {
    // Prefer real history so the browser's own Back stays consistent; fall back to
    // the list when this page was opened straight from a pasted URL.
    if (history.length > 1) { history.back(); return; }
    history.pushState({ page: 'users' }, '', '#users');
    this._resetDetail();
  }

  _patchD(field, value) {
    this._dForm = { ...this._dForm, [field]: value };
    this._dSaved = false;
  }

  _iconFailed(name) {
    const next = new Set(this._noIcon);
    next.add(name);
    this._noIcon = next;
  }

  // ── Create (the one remaining modal) ─────────────────────────────────────────

  _openCreate() {
    this._modal = {
      mode: 'create',
      form: { username: '', display_name: '', role_id: this._roles?.[0]?.id ?? '', password: '', encrypted: false, birthdate: '', sex: '', notes: '' },
    };
  }

  _closeModal() { this._modal = null; this._error = null; }

  _patch(field, value) {
    this._modal = { ...this._modal, form: { ...this._modal.form, [field]: value } };
  }

  async _saveCreate() {
    const { form } = this._modal;
    this._error = null;
    if (!form.username.trim() || !form.password) { this._error = t('users.error.required_username_pw'); return; }
    try {
      const res = await fetch('/api/users', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          username: form.username.trim(),
          display_name: form.display_name.trim() || null,
          role_id: form.role_id,
          password: form.password,
          encrypted: form.encrypted,
          birthdate: form.birthdate || null,
          sex: form.sex.trim() || null,
          notes: form.notes.trim() || null,
        }),
      });
      if (!res.ok) throw new Error(await res.text());
      this._closeModal();
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  // ── Detail: profile ───────────────────────────────────────────────────────────

  async _saveProfile() {
    const u = this._user;
    const f = this._dForm;
    if (!f.username.trim()) { this._error = t('users.error.required_username'); return; }
    this._busy = true; this._error = null;
    try {
      const res = await fetch(`/api/users/${encodeURIComponent(u.id)}`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          username: f.username.trim(),
          display_name: f.display_name.trim() || null,
          role_id: f.role_id,
          active: f.active,
          birthdate: f.birthdate || null,
          sex: f.sex.trim() || null,
          notes: f.notes.trim() || null,
        }),
      });
      if (!res.ok) throw new Error(await res.text());
      await this._load();
      this._dSaved = true;
    } catch (e) { this._error = e.message; }
    finally { this._busy = false; }
  }

  // ── Detail: connectors ───────────────────────────────────────────────────────

  _toggleConn(idx) {
    this._conns = this._conns.map((c, i) => i === idx ? { ...c, granted: !c.granted } : c);
    this._connSaved = false;
  }

  async _saveConnectors() {
    const u = this._user;
    const global_ids    = this._conns.filter(c => c.kind === 'global'  && c.granted).map(c => c.id);
    const catalog_names = this._conns.filter(c => c.kind === 'catalog' && c.granted).map(c => c.name);
    this._busy = true; this._error = null;
    try {
      const res = await fetch(`/api/users/${encodeURIComponent(u.id)}/connectors`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ global_ids, catalog_names }),
      });
      if (!res.ok) throw new Error(await res.text());
      this._connSaved = true;
    } catch (e) { this._error = e.message; }
    finally { this._busy = false; }
  }

  // ── Detail: plugins ──────────────────────────────────────────────────────────

  _togglePlug(idx) {
    this._plugs = this._plugs.map((p, i) => i === idx ? { ...p, granted: !p.granted } : p);
    this._plugSaved = false;
  }

  async _savePlugins() {
    const u = this._user;
    const plugin_ids = this._plugs.filter(p => p.granted).map(p => p.id);
    this._busy = true; this._error = null;
    try {
      const res = await fetch(`/api/users/${encodeURIComponent(u.id)}/plugins`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ plugin_ids }),
      });
      if (!res.ok) throw new Error(await res.text());
      this._plugSaved = true;
    } catch (e) { this._error = e.message; }
    finally { this._busy = false; }
  }

  // ── Detail: event-triage schedule ────────────────────────────────────────────

  async _saveTriage() {
    const u = this._user;
    const raw = this._triageIn.trim();
    // Empty is a value, not a missing one: it clears the override and puts this
    // person back on the instance schedule. Hence `null` rather than an omitted
    // field, and hence no "use default" checkbox — the empty box says it.
    let interval_minutes = null;
    if (raw !== '') {
      const n = Number(raw);
      if (!Number.isInteger(n) || n < 1) { this._error = t('users.triage.invalid'); return; }
      interval_minutes = n;
    }
    this._busy = true; this._error = null;
    try {
      const res = await fetch(`/api/users/${encodeURIComponent(u.id)}/event-triage`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ interval_minutes }),
      });
      if (!res.ok) throw new Error(await res.text());
      this._triage = await res.json();
      this._trgSaved = true;
    } catch (e) { this._error = e.message; }
    finally { this._busy = false; }
  }

  // ── Detail: security ──────────────────────────────────────────────────────────

  async _resetPassword() {
    const u = this._user;
    if (!this._dPw) { this._error = t('users.error.password_empty'); return; }
    this._busy = true; this._error = null;
    try {
      const res = await fetch(`/api/users/${encodeURIComponent(u.id)}/password`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ password: this._dPw }),
      });
      if (!res.ok) throw new Error(await res.text());
      this._dPw = '';
      this._pwSaved = true;
    } catch (e) { this._error = e.message; }
    finally { this._busy = false; }
  }

  async _deleteUser() {
    const u = this._user;
    if (!confirm(t('users.confirm.delete', { username: u.username }))) return;
    this._busy = true; this._error = null;
    try {
      const res = await fetch(`/api/users/${encodeURIComponent(u.id)}`, { method: 'DELETE' });
      if (!res.ok) throw new Error(await res.text());
      history.pushState({ page: 'users' }, '', '#users');
      this._resetDetail();
      await this._load();
    } catch (e) { this._error = e.message; this._busy = false; }
  }

  // ── Render ──────────────────────────────────────────────────────────────────

  render() {
    if (!this._open) return nothing;
    return html`
      ${this._view === 'user' ? this._renderDetail() : this._renderList()}
      ${this._renderCreateModal()}
    `;
  }

  _renderList() {
    const users = this._users ?? [];
    const loading = this._users === null;

    return html`
      <div class="um-page">
        <div class="page-header">
          <div class="page-header-left">
            <h2 class="page-header-title"><i class="bi bi-people-fill me-2"></i>${t('users.title')}</h2>
          </div>
          <div class="page-header-actions">
            <span class="page-header-count">${t(users.length === 1 ? 'users.count_one' : 'users.count_other', { n: users.length })}</span>
            <button class="btn btn-sm btn-primary" @click=${() => this._openCreate()}>
              <i class="bi bi-plus-lg me-1"></i>${t('users.btn.new')}
            </button>
          </div>
        </div>

        ${this._error && !this._modal ? html`
          <div class="alert alert-danger py-2 mx-4" style="font-size:.85rem">${this._error}</div>` : nothing}

        <div class="um-table-wrap">
          ${loading ? html`<div class="um-empty"><i class="bi bi-hourglass-split"></i> ${t('users.loading')}</div>` : users.length === 0 ? html`
            <div class="um-empty"><i class="bi bi-people"></i><p>${t('users.empty')}</p></div>
          ` : html`
            <table class="um-table um-table-clickable">
              <thead>
                <tr>
                  <th>${t('users.table.username')}</th>
                  <th>${t('users.table.display_name')}</th>
                  <th>${t('users.table.role')}</th>
                  <th>${t('users.table.db')}</th>
                  <th>${t('users.table.status')}</th>
                  <th></th>
                </tr>
              </thead>
              <tbody>
                ${users.map(u => html`
                  <tr @click=${() => this._openUser(u)}>
                    <td>
                      <div class="d-flex align-items-center gap-2">
                        <span class="ud-avatar ud-avatar--sm" style="background:${avatarColor(u.username)}">
                          ${(u.display_name || u.username).charAt(0).toUpperCase()}
                        </span>
                        <strong>${u.username}</strong>
                      </div>
                    </td>
                    <td>${u.display_name ?? '—'}</td>
                    <td>${this._roleLabel(u.role_id)}</td>
                    <td>${u.encrypted
                      ? html`<span class="um-badge um-badge-encrypted">${t('users.badge.encrypted')}</span>`
                      : html`<span class="um-badge um-badge-clear">${t('users.badge.cleartext')}</span>`}</td>
                    <td>${u.active
                      ? html`<span class="um-badge um-badge-active">${t('users.badge.active')}</span>`
                      : html`<span class="um-badge um-badge-inactive">${t('users.badge.inactive')}</span>`}</td>
                    <td style="width:1rem"><i class="bi bi-chevron-right text-muted" style="font-size:.75rem"></i></td>
                  </tr>
                `)}
              </tbody>
            </table>
          `}
        </div>
      </div>
    `;
  }

  _roleLabel(roleId) {
    return this._roles?.find(r => r.id === roleId)?.label ?? roleId;
  }

  // ── Render: the user's own page ────────────────────────────────────────────

  _renderDetail() {
    const u = this._user;
    if (!u || !this._dForm) {
      return html`
        <div class="um-page">
          ${this._renderDetailHeader(null)}
          ${this._error
            ? html`<div class="alert alert-danger py-2 mx-4" style="font-size:.85rem">${this._error}</div>`
            : html`<div class="um-empty"><i class="bi bi-hourglass-split"></i> ${t('users.loading')}</div>`}
        </div>`;
    }
    return html`
      <div class="um-page">
        ${this._renderDetailHeader(u)}
        <div style="padding:0 1.25rem 2rem; overflow:auto; max-width:860px">
          ${this._error ? html`
            <div class="alert alert-danger py-2 mb-3" style="font-size:.85rem">${this._error}</div>` : nothing}
          ${this._renderProfile(u)}
          ${this._renderConnectors(u)}
          ${this._renderPlugins(u)}
          ${this._renderTriage(u)}
          ${this._renderSecurity(u)}
        </div>
      </div>`;
  }

  _renderDetailHeader(u) {
    return html`
      <div class="page-header">
        <div class="page-header-left">
          <button class="btn btn-sm btn-outline-secondary page-header-back" title=${t('users.detail.back')} @click=${() => this._back()}>
            <i class="bi bi-arrow-left"></i>
          </button>
          ${u ? html`
            <span class="ud-avatar" style="background:${avatarColor(u.username)}">
              ${(u.display_name || u.username).charAt(0).toUpperCase()}
            </span>
            <div style="min-width:0">
              <h2 class="page-header-title" style="min-width:0;overflow:hidden;text-overflow:ellipsis">
                ${u.display_name || u.username}</h2>
              <div class="d-flex align-items-center gap-2" style="font-size:.72rem">
                <code class="text-muted">${u.username}</code>
                <span class="text-muted">·</span>
                <span class="text-muted">${this._roleLabel(u.role_id)}</span>
                ${u.active ? nothing : html`<span class="um-badge um-badge-inactive">${t('users.badge.inactive')}</span>`}
              </div>
            </div>`
          : html`<h2 class="page-header-title">${t('users.title')}</h2>`}
        </div>
      </div>`;
  }

  _renderProfile(u) {
    const f = this._dForm;
    return html`
      <div class="ud-section">
        <h3 class="ud-section-title"><i class="bi bi-person me-2"></i>${t('users.detail.profile')}</h3>
        <div class="connector-card">
          <div class="row g-3">
            <div class="col-md-6">
              <label class="form-label">${t('users.modal.username')}</label>
              <input class="form-control form-control-sm" .value=${f.username} @input=${e => this._patchD('username', e.target.value)} />
            </div>
            <div class="col-md-6">
              <label class="form-label">${t('users.modal.display_name')} <span class="text-muted">${t('users.modal.optional')}</span></label>
              <input class="form-control form-control-sm" .value=${f.display_name} @input=${e => this._patchD('display_name', e.target.value)} />
            </div>
            <div class="col-md-6">
              <label class="form-label">${t('users.modal.role')}</label>
              <select class="form-select form-select-sm" @change=${e => this._patchD('role_id', e.target.value)}>
                ${(this._roles ?? []).map(r => html`<option value=${r.id} ?selected=${f.role_id === r.id}>${r.label}</option>`)}
              </select>
            </div>
            <div class="col-md-3">
              <label class="form-label">${t('users.modal.birthdate')} <span class="text-muted">${t('users.modal.optional')}</span></label>
              <input type="date" class="form-control form-control-sm" .value=${f.birthdate} @input=${e => this._patchD('birthdate', e.target.value)} />
            </div>
            <div class="col-md-3">
              <label class="form-label">${t('users.modal.sex')} <span class="text-muted">${t('users.modal.optional')}</span></label>
              <input class="form-control form-control-sm" .value=${f.sex} @input=${e => this._patchD('sex', e.target.value)} />
            </div>
            <div class="col-12">
              <label class="form-label">${t('users.modal.notes')} <span class="text-muted">${t('users.modal.optional')}</span></label>
              <textarea class="form-control form-control-sm" rows="3" .value=${f.notes} @input=${e => this._patchD('notes', e.target.value)}></textarea>
              <div class="form-text">${t('users.modal.notes_hint')}</div>
            </div>
            <div class="col-12">
              <div class="form-check form-switch">
                <input class="form-check-input" type="checkbox" id="ud-active"
                  .checked=${f.active} @change=${e => this._patchD('active', e.target.checked)} />
                <label class="form-check-label" for="ud-active">${t('users.modal.active')}</label>
              </div>
            </div>
          </div>
          <div class="d-flex align-items-center gap-2 mt-1">
            <button class="btn btn-sm btn-primary" ?disabled=${this._busy} @click=${() => this._saveProfile()}>
              <i class="bi bi-check-lg me-1"></i>${t('users.modal.save_btn')}
            </button>
            ${this._dSaved ? html`<span class="ud-saved"><i class="bi bi-check2"></i>${t('users.detail.saved')}</span>` : nothing}
          </div>
        </div>
      </div>`;
  }

  _renderConnectors(u) {
    const conns = this._conns;
    const q = this._connQ.trim().toLowerCase();
    const visible = (conns ?? []).filter(c => !q
      || c.name.toLowerCase().includes(q)
      || (c.friendly_name ?? '').toLowerCase().includes(q)
      || (c.description ?? '').toLowerCase().includes(q));
    const globals = visible.filter(c => c.kind === 'global');
    const catalog = visible.filter(c => c.kind === 'catalog');

    const row = (c) => {
      const idx = this._conns.indexOf(c);
      const showIcon = !this._noIcon.has(c.name);
      return html`
        <label class="connector-row" style="cursor:pointer">
          <input class="form-check-input" type="checkbox"
            .checked=${c.granted} @change=${() => this._toggleConn(idx)} />
          ${showIcon
            ? html`<img class="connector-card-icon" src=${connectorIconUrl(c.name, 'sm')} alt=""
                     @error=${() => this._iconFailed(c.name)} />`
            : html`<div class="connector-card-icon connector-card-icon--empty"><i class="bi bi-plug"></i></div>`}
          <div class="connector-row-main">
            <div class="connector-row-name">
              <span>${c.friendly_name || c.name}</span>
              ${c.friendly_name ? html`<span class="connector-row-sub">${c.name}</span>` : nothing}
            </div>
            ${c.description ? html`<div class="connector-row-desc">${c.description}</div>` : nothing}
          </div>
          <div class="connector-row-chips">
            ${c.kind === 'global' ? html`
              <span class="connector-chip"><i class="bi bi-globe"></i>${t('connectors.chip.global')}</span>` : nothing}
            ${!c.enabled ? html`
              <span class="connector-chip"><i class="bi bi-pause-circle"></i>${t('users.conn.disabled')}</span>` : nothing}
          </div>
        </label>`;
    };

    return html`
      <div class="ud-section">
        <h3 class="ud-section-title"><i class="bi bi-plug me-2"></i>${t('users.detail.connectors')}</h3>
        ${conns === null
          ? html`<div class="um-empty" style="padding:1rem"><i class="bi bi-hourglass-split"></i> ${t('users.loading')}</div>`
          : conns.length === 0
            ? html`<div class="um-empty" style="padding:1rem"><i class="bi bi-plug"></i><p>${t('users.conn.empty')}</p></div>`
            : html`
              <div class="connector-filters">
                <div class="connector-search">
                  <i class="bi bi-search"></i>
                  <input class="form-control form-control-sm" placeholder=${t('connectors.search')}
                    .value=${this._connQ} @input=${(e) => { this._connQ = e.target.value; }} />
                </div>
              </div>
              ${visible.length === 0
                ? html`<div class="um-empty" style="padding:1rem"><i class="bi bi-search"></i>
                    <p>${t('connectors.empty.match', { query: this._connQ })}</p></div>`
                : html`
                  ${globals.length ? html`
                    <div class="ud-conn-group-title">${t('users.conn.globals')}</div>
                    <div class="form-text mb-2" style="font-size:.75rem">${t('users.conn.hint_global')}</div>
                    <div class="connector-list" style="margin-bottom:1rem">${globals.map(row)}</div>` : nothing}
                  ${catalog.length ? html`
                    <div class="ud-conn-group-title">${t('users.conn.catalog')}</div>
                    <div class="form-text mb-2" style="font-size:.75rem">${t('users.conn.hint_catalog')}</div>
                    <div class="connector-list">${catalog.map(row)}</div>` : nothing}`}
              <div class="d-flex align-items-center gap-2 mt-3">
                <button class="btn btn-sm btn-primary" ?disabled=${this._busy} @click=${() => this._saveConnectors()}>
                  <i class="bi bi-check-lg me-1"></i>${t('users.modal.save_btn')}
                </button>
                ${this._connSaved ? html`<span class="ud-saved"><i class="bi bi-check2"></i>${t('users.detail.saved')}</span>` : nothing}
              </div>`}
      </div>`;
  }

  // Deliberately unfiltered, unlike the connectors above: a plugin ships in the
  // binary, so the list is short and a search box over it is furniture. Plugins
  // that gate access through their own pairing (Mobile Connector) never reach
  // here — the server omits them, since a checkbox would control nothing.
  _renderPlugins(u) {
    const plugs = this._plugs;
    // An admin holds every enabled plugin implicitly (`list_accessible` short-
    // circuits on the role), so unticked boxes here would read as "no access".
    const isAdmin = u.role_id === 'admin';
    return html`
      <div class="ud-section">
        <h3 class="ud-section-title"><i class="bi bi-puzzle me-2"></i>${t('users.detail.plugins')}</h3>
        ${isAdmin ? html`
          <div class="alert alert-info py-2 mb-2" style="font-size:.8rem">
            <i class="bi bi-info-circle me-1"></i>${t('users.plug.admin_note')}
          </div>` : nothing}
        ${plugs === null
          ? html`<div class="um-empty" style="padding:1rem"><i class="bi bi-hourglass-split"></i> ${t('users.loading')}</div>`
          : plugs.length === 0
            ? html`<div class="um-empty" style="padding:1rem"><i class="bi bi-puzzle"></i><p>${t('users.plug.empty')}</p></div>`
            : html`
              <div class="form-text mb-2" style="font-size:.75rem">${t('users.plug.hint')}</div>
              <div class="connector-list">
                ${plugs.map((p, i) => html`
                  <label class="connector-row" style="cursor:pointer">
                    <input class="form-check-input" type="checkbox"
                      .checked=${p.granted} @change=${() => this._togglePlug(i)} />
                    <div class="connector-card-icon connector-card-icon--empty"><i class="bi bi-puzzle"></i></div>
                    <div class="connector-row-main">
                      <div class="connector-row-name">
                        <span>${p.name}</span>
                        <span class="connector-row-sub">${p.id}</span>
                      </div>
                      ${p.description ? html`<div class="connector-row-desc">${p.description}</div>` : nothing}
                    </div>
                    <div class="connector-row-chips">
                      ${p.enabled ? nothing : html`
                        <span class="connector-chip"><i class="bi bi-pause-circle"></i>${t('users.conn.disabled')}</span>`}
                    </div>
                  </label>`)}
              </div>
              <div class="d-flex align-items-center gap-2 mt-3">
                <button class="btn btn-sm btn-primary" ?disabled=${this._busy} @click=${() => this._savePlugins()}>
                  <i class="bi bi-check-lg me-1"></i>${t('users.modal.save_btn')}
                </button>
                ${this._plugSaved ? html`<span class="ud-saved"><i class="bi bi-check2"></i>${t('users.detail.saved')}</span>` : nothing}
              </div>`}
      </div>`;
  }

  // The one *schedule* on this page, and the only agent that gets one: event
  // triage fires on inbound events, so how often it runs is a fact about the
  // person, not about the instance. Someone on a dozen mailing lists is triaged
  // on nearly every tick.
  _renderTriage(u) {
    if (!this._triage) return nothing;   // not an admin, or the fetch failed
    const def = this._triage.default_interval_minutes;
    return html`
      <div class="ud-section">
        <h3 class="ud-section-title"><i class="bi bi-clock-history me-2"></i>${t('users.detail.triage')}</h3>
        <div class="connector-card">
          <div class="form-text mb-2" style="font-size:.75rem">${t('users.triage.hint')}</div>
          <div class="row g-2 align-items-end">
            <div class="col-md-5">
              <label class="form-label">${t('users.triage.interval')}</label>
              <input type="number" min="1" max="1440" class="form-control form-control-sm"
                placeholder=${t('users.triage.placeholder', { n: def })}
                .value=${this._triageIn}
                @input=${(e) => { this._triageIn = e.target.value; this._trgSaved = false; }} />
              <div class="form-text">${this._triageIn.trim() === ''
                ? t('users.triage.using_default', { n: def })
                : t('users.triage.using_override')}</div>
            </div>
            <div class="col-auto d-flex align-items-center gap-2 pb-4">
              <button class="btn btn-sm btn-primary" ?disabled=${this._busy} @click=${() => this._saveTriage()}>
                <i class="bi bi-check-lg me-1"></i>${t('users.modal.save_btn')}
              </button>
              ${this._trgSaved ? html`<span class="ud-saved"><i class="bi bi-check2"></i>${t('users.detail.saved')}</span>` : nothing}
            </div>
          </div>
        </div>
      </div>`;
  }

  _renderSecurity(u) {
    return html`
      <div class="ud-section">
        <h3 class="ud-section-title"><i class="bi bi-shield-lock me-2"></i>${t('users.detail.security')}</h3>
        <div class="connector-card">
          ${u.encrypted ? html`
            <div class="alert alert-warning py-2 mb-0" style="font-size:.82rem">
              <i class="bi bi-exclamation-triangle me-1"></i>${t('users.modal.only_cleartext')}
            </div>` : html`
            <div class="row g-2 align-items-end">
              <div class="col-md-6">
                <label class="form-label">${t('users.modal.new_password')}</label>
                <input type="password" class="form-control form-control-sm" .value=${this._dPw}
                  @input=${(e) => { this._dPw = e.target.value; this._pwSaved = false; }} />
              </div>
              <div class="col-auto d-flex align-items-center gap-2">
                <button class="btn btn-sm btn-outline-secondary" ?disabled=${this._busy || !this._dPw}
                  @click=${() => this._resetPassword()}>
                  <i class="bi bi-key me-1"></i>${t('users.modal.reset_btn')}
                </button>
                ${this._pwSaved ? html`<span class="ud-saved"><i class="bi bi-check2"></i>${t('users.detail.saved')}</span>` : nothing}
              </div>
            </div>`}
        </div>

        <div class="connector-card ud-danger">
          <div class="d-flex align-items-center justify-content-between gap-3 flex-wrap">
            <div style="min-width:0;font-size:.8rem" class="text-muted">${t('users.detail.delete_hint')}</div>
            <button class="btn btn-sm btn-outline-danger" ?disabled=${this._busy} @click=${() => this._deleteUser()}>
              <i class="bi bi-trash me-1"></i>${t('users.action.delete')}
            </button>
          </div>
        </div>
      </div>`;
  }

  // ── Render: create modal ─────────────────────────────────────────────────────

  _profileFields(form) {
    return html`
      <div class="mb-3">
        <label class="form-label">${t('users.modal.birthdate')} <span class="text-muted">${t('users.modal.optional')}</span></label>
        <input type="date" class="form-control" .value=${form.birthdate} @input=${e => this._patch('birthdate', e.target.value)} />
      </div>
      <div class="mb-3">
        <label class="form-label">${t('users.modal.sex')} <span class="text-muted">${t('users.modal.optional')}</span></label>
        <input class="form-control" .value=${form.sex} @input=${e => this._patch('sex', e.target.value)} />
      </div>
      <div class="mb-3">
        <label class="form-label">${t('users.modal.notes')} <span class="text-muted">${t('users.modal.optional')}</span></label>
        <textarea class="form-control" rows="3" .value=${form.notes} @input=${e => this._patch('notes', e.target.value)}></textarea>
        <div class="form-text">${t('users.modal.notes_hint')}</div>
      </div>
    `;
  }

  _renderCreateModal() {
    if (!this._modal) return nothing;
    const { form } = this._modal;
    return html`
      <div class="um-modal-overlay" @click=${(e) => { if (e.target.classList.contains('um-modal-overlay')) this._closeModal(); }}>
        <div class="um-modal">
          <div class="um-modal-header">
            <i class="bi bi-person-plus"></i>
            <span>${t('users.modal.create_title')}</span>
            <button class="um-btn-icon ms-auto" @click=${() => this._closeModal()}><i class="bi bi-x-lg"></i></button>
          </div>
          <div class="um-modal-body">
            ${this._error ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:.85rem">${this._error}</div>` : nothing}
            <div class="mb-3">
              <label class="form-label">${t('users.modal.username')}</label>
              <input class="form-control" .value=${form.username} @input=${e => this._patch('username', e.target.value)} />
            </div>
            <div class="mb-3">
              <label class="form-label">${t('users.modal.display_name')} <span class="text-muted">${t('users.modal.optional')}</span></label>
              <input class="form-control" .value=${form.display_name} @input=${e => this._patch('display_name', e.target.value)} />
            </div>
            <div class="mb-3">
              <label class="form-label">${t('users.modal.role')}</label>
              <select class="form-select" @change=${e => this._patch('role_id', e.target.value)}>
                ${(this._roles ?? []).map(r => html`<option value=${r.id} ?selected=${form.role_id === r.id}>${r.label}</option>`)}
              </select>
            </div>
            ${this._profileFields(form)}
            <div class="mb-3">
              <label class="form-label">${t('users.modal.password')}</label>
              <input type="password" class="form-control" .value=${form.password} @input=${e => this._patch('password', e.target.value)} />
            </div>
            <div class="form-check">
              <input class="form-check-input" type="checkbox" id="um-enc"
                .checked=${form.encrypted}
                @change=${e => this._patch('encrypted', e.target.checked)} />
              <label class="form-check-label" for="um-enc">${t('users.modal.encrypt')}</label>
            </div>
            ${form.encrypted ? html`
              <div class="setup-warn mt-2">${unsafeHTML(t('users.modal.encrypt_warn'))}</div>
            ` : nothing}
          </div>
          <div class="um-modal-footer">
            <button class="btn btn-sm btn-outline-secondary" @click=${() => this._closeModal()}>${t('users.modal.cancel')}</button>
            <button class="btn btn-sm btn-primary" @click=${() => this._saveCreate()}>
              <i class="bi bi-check-lg me-1"></i>${t('users.modal.create_btn')}
            </button>
          </div>
        </div>
      </div>
    `;
  }
}
