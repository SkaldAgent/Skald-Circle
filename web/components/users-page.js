import { html, nothing }          from 'lit';
import { unsafeHTML }             from 'lit/directives/unsafe-html.js';
import { LightElement }           from '../lib/base.js';
import { t }                      from '../lib/i18n.js';

export class UsersPage extends LightElement {

  static get properties() {
    return {
      _open:     { state: true },
      _users:    { state: true },
      _roles:    { state: true },
      _error:    { state: true },
      _modal:    { state: true },  // null | { mode: 'create'|'edit'|'password', user?, form }
    };
  }

  constructor() {
    super();
    this._open  = false;
    this._users = null;
    this._roles = null;
    this._error = null;
    this._modal = null;
  }

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'users';
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
      const [uRes, rRes] = await Promise.all([
        fetch('/api/users'),
        fetch('/api/roles'),
      ]);
      if (!uRes.ok) throw new Error(`Users: HTTP ${uRes.status}`);
      if (!rRes.ok) throw new Error(`Roles: HTTP ${rRes.status}`);
      this._users = await uRes.json();
      this._roles = await rRes.json();
    } catch (e) {
      this._error = e.message;
    }
  }

  // ── Modal helpers ────────────────────────────────────────────────────────────

  _openCreate() {
    this._modal = {
      mode: 'create',
      form: { username: '', display_name: '', role_id: this._roles?.[0]?.id ?? '', password: '', encrypted: false, birthdate: '', sex: '', notes: '' },
    };
  }

  _openEdit(user) {
    this._modal = {
      mode: 'edit',
      user,
      form: { username: user.username, display_name: user.display_name ?? '', role_id: user.role_id, active: user.active, birthdate: user.birthdate ?? '', sex: user.sex ?? '', notes: user.notes ?? '' },
    };
  }

  _openPassword(user) {
    this._modal = { mode: 'password', user, form: { password: '' } };
  }

  _closeModal() { this._modal = null; this._error = null; }

  _patch(field, value) {
    this._modal = { ...this._modal, form: { ...this._modal.form, [field]: value } };
  }

  // ── API actions ──────────────────────────────────────────────────────────────

  async _save() {
    const { mode, form } = this._modal;
    this._error = null;

    if (mode === 'create') {
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
    } else if (mode === 'edit') {
      const { user } = this._modal;
      if (!form.username.trim()) { this._error = t('users.error.required_username'); return; }
      try {
        const res = await fetch(`/api/users/${user.id}`, {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            username: form.username.trim(),
            display_name: form.display_name.trim() || null,
            role_id: form.role_id,
            active: form.active,
            birthdate: form.birthdate || null,
            sex: form.sex.trim() || null,
            notes: form.notes.trim() || null,
          }),
        });
        if (!res.ok) throw new Error(await res.text());
        this._closeModal();
        await this._load();
      } catch (e) { this._error = e.message; }
    } else if (mode === 'password') {
      const { user } = this._modal;
      if (!form.password) { this._error = t('users.error.password_empty'); return; }
      try {
        const res = await fetch(`/api/users/${user.id}/password`, {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ password: form.password }),
        });
        if (!res.ok) throw new Error(await res.text());
        this._closeModal();
      } catch (e) { this._error = e.message; }
    }
  }

  async _delete(user) {
    if (!confirm(t('users.confirm.delete', { username: user.username }))) return;
    try {
      const res = await fetch(`/api/users/${user.id}`, { method: 'DELETE' });
      if (!res.ok) throw new Error(await res.text());
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  // ── Render ──────────────────────────────────────────────────────────────────

  _roleLabel(roleId) {
    return this._roles?.find(r => r.id === roleId)?.label ?? roleId;
  }

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

  _renderModal() {
    if (!this._modal) return nothing;
    const { mode, form, user } = this._modal;
    const title = mode === 'create' ? t('users.modal.create_title')
                : mode === 'edit'   ? t('users.modal.edit_title', { username: user.username })
                :                     t('users.modal.reset_title', { username: user.username });

    return html`
      <div class="um-modal-overlay" @click=${(e) => { if (e.target.classList.contains('um-modal-overlay')) this._closeModal(); }}>
        <div class="um-modal">
          <div class="um-modal-header">
            <i class="bi ${mode === 'create' ? 'bi-person-plus' : mode === 'edit' ? 'bi-pencil-square' : 'bi-key'}"></i>
            <span>${title}</span>
            <button class="um-btn-icon ms-auto" @click=${() => this._closeModal()}><i class="bi bi-x-lg"></i></button>
          </div>
          <div class="um-modal-body">
            ${this._error ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:.85rem">${this._error}</div>` : nothing}

            ${mode === 'create' ? html`
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
            ` : mode === 'edit' ? html`
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
              <div class="form-check">
                <input class="form-check-input" type="checkbox" id="um-active"
                  .checked=${form.active}
                  @change=${e => this._patch('active', e.target.checked)} />
                <label class="form-check-label" for="um-active">${t('users.modal.active')}</label>
              </div>
            ` : html`
              <div class="alert alert-warning py-2 mb-3" style="font-size:.82rem">
                <i class="bi bi-exclamation-triangle me-1"></i>${t('users.modal.only_cleartext')}
              </div>
              <div class="mb-3">
                <label class="form-label">${t('users.modal.new_password')}</label>
                <input type="password" class="form-control" .value=${form.password} @input=${e => this._patch('password', e.target.value)} />
              </div>
            `}
          </div>
          <div class="um-modal-footer">
            <button class="btn btn-sm btn-outline-secondary" @click=${() => this._closeModal()}>${t('users.modal.cancel')}</button>
            <button class="btn btn-sm btn-primary" @click=${() => this._save()}>
              <i class="bi bi-check-lg me-1"></i>${mode === 'create' ? t('users.modal.create_btn') : mode === 'edit' ? t('users.modal.save_btn') : t('users.modal.reset_btn')}
            </button>
          </div>
        </div>
      </div>
    `;
  }

  render() {
    if (!this._open) return nothing;
    const users = this._users ?? [];
    const loading = this._users === null;

    return html`
      <div class="um-page">
        <div class="um-header">
          <h2 class="um-title"><i class="bi bi-people-fill me-2"></i>${t('users.title')}</h2>
          <div class="um-header-right">
            <span class="um-header-count">${t(users.length === 1 ? 'users.count_one' : 'users.count_other', { n: users.length })}</span>
            <button class="btn btn-sm btn-primary" @click=${() => this._openCreate()}>
              <i class="bi bi-plus-lg me-1"></i>${t('users.btn.new')}
            </button>
          </div>
        </div>

        ${this._error && !this._modal ? html`
          <div class="alert alert-danger py-2 mx-4" style="font-size:.85rem">${this._error}</div>
        ` : nothing}

        <div class="um-table-wrap">
          ${loading ? html`<div class="um-empty"><i class="bi bi-hourglass-split"></i> ${t('users.loading')}</div>` : users.length === 0 ? html`
            <div class="um-empty"><i class="bi bi-people"></i><p>${t('users.empty')}</p></div>
          ` : html`
            <table class="um-table">
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
                  <tr>
                    <td><strong>${u.username}</strong></td>
                    <td>${u.display_name ?? '—'}</td>
                    <td>${this._roleLabel(u.role_id)}</td>
                    <td>${u.encrypted
                      ? html`<span class="um-badge um-badge-encrypted">${t('users.badge.encrypted')}</span>`
                      : html`<span class="um-badge um-badge-clear">${t('users.badge.cleartext')}</span>`}</td>
                    <td>${u.active
                      ? html`<span class="um-badge um-badge-active">${t('users.badge.active')}</span>`
                      : html`<span class="um-badge um-badge-inactive">${t('users.badge.inactive')}</span>`}</td>
                    <td>
                      <div class="um-actions">
                        <button class="um-btn-icon" title=${t('users.action.reset_pw')} @click=${() => this._openPassword(u)}>
                          <i class="bi bi-key"></i>
                        </button>
                        <button class="um-btn-icon" title=${t('users.action.edit')} @click=${() => this._openEdit(u)}>
                          <i class="bi bi-pencil"></i>
                        </button>
                        <button class="um-btn-icon" title=${t('users.action.delete')} @click=${() => this._delete(u)}>
                          <i class="bi bi-trash"></i>
                        </button>
                      </div>
                    </td>
                  </tr>
                `)}
              </tbody>
            </table>
          `}
        </div>
      </div>
      ${this._renderModal()}
    `;
  }
}
