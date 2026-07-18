import { html, nothing } from 'lit';
import { unsafeHTML }     from 'lit/directives/unsafe-html.js';
import { LightElement } from '../lib/base.js';
import { t }            from '../lib/i18n.js';

const ADMIN_ID = 'admin';

export class RolesPage extends LightElement {

  static get properties() {
    return {
      _open:    { state: true },
      _roles:   { state: true },
      _groups:  { state: true },
      _error:   { state: true },
      _modal:   { state: true },  // null | { mode: 'create'|'edit', role?, form }
    };
  }

  constructor() {
    super();
    this._open   = false;
    this._roles  = null;
    this._groups = null;
    this._error  = null;
    this._modal  = null;
  }

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'roles';
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
      const [rRes, gRes] = await Promise.all([
        fetch('/api/roles'),
        fetch('/api/tool-permission-groups'),
      ]);
      if (!rRes.ok) throw new Error(`HTTP ${rRes.status}`);
      if (!gRes.ok) throw new Error(`HTTP ${gRes.status}`);
      this._roles  = await rRes.json();
      this._groups = await gRes.json();
    } catch (e) {
      this._error = e.message;
    }
  }

  // ── Modal helpers ────────────────────────────────────────────────────────────

  // `ui_mode` lives in the free-form attrs JSON (data-driven, §0.1): the UI
  // surfaces it as a first-class select without hardcoding any role semantics.
  _attrsUiMode(attrs) {
    try { return JSON.parse(attrs || '{}').ui_mode === 'simple' ? 'simple' : 'full'; }
    catch { return 'full'; }
  }

  _mergeAttrs(attrs, uiMode) {
    let o = {};
    try { o = JSON.parse(attrs || '{}') ?? {}; } catch { o = {}; }
    if (uiMode === 'simple') o.ui_mode = 'simple'; else delete o.ui_mode;
    const keys = Object.keys(o);
    return keys.length ? JSON.stringify(o) : null;
  }

  _openCreate() {
    this._modal = {
      mode: 'create',
      form: { id: '', label: '', permission_group: this._groups?.[0]?.id ?? 'default', attrs: '', ui_mode: 'full' },
    };
  }

  _openEdit(role) {
    this._modal = {
      mode: 'edit',
      role,
      form: { label: role.label, permission_group: role.permission_group, attrs: role.attrs ?? '', ui_mode: this._attrsUiMode(role.attrs) },
    };
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
      if (!form.id.trim() || !form.label.trim()) { this._error = t('roles.error.id_label'); return; }
      try {
        const res = await fetch('/api/roles', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            id: form.id.trim(),
            label: form.label.trim(),
            permission_group: form.permission_group,
            attrs: this._mergeAttrs(form.attrs, form.ui_mode),
          }),
        });
        if (!res.ok) throw new Error(await res.text());
        this._closeModal();
        await this._load();
      } catch (e) { this._error = e.message; }
    } else {
      const { role } = this._modal;
      if (!form.label.trim()) { this._error = t('roles.error.label'); return; }
      try {
        const res = await fetch(`/api/roles/${role.id}`, {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            label: form.label.trim(),
            permission_group: form.permission_group,
            attrs: this._mergeAttrs(form.attrs, form.ui_mode),
          }),
        });
        if (!res.ok) throw new Error(await res.text());
        this._closeModal();
        await this._load();
      } catch (e) { this._error = e.message; }
    }
  }

  async _delete(role) {
    if (!confirm(t('roles.confirm.delete', { name: role.label }))) return;
    try {
      const res = await fetch(`/api/roles/${role.id}`, { method: 'DELETE' });
      if (!res.ok) throw new Error(await res.text());
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  // ── Render ──────────────────────────────────────────────────────────────────

  _groupLabel(groupId) {
    return this._groups?.find(g => g.id === groupId)?.name ?? groupId;
  }

  _renderModal() {
    if (!this._modal) return nothing;
    const { mode, form, role } = this._modal;
    const title = mode === 'create' ? t('roles.form.new') : t('roles.form.edit', { name: role.label });

    return html`
      <div class="um-modal-overlay" @click=${(e) => { if (e.target.classList.contains('um-modal-overlay')) this._closeModal(); }}>
        <div class="um-modal">
          <div class="um-modal-header">
            <i class="bi ${mode === 'create' ? 'bi-plus-circle' : 'bi-pencil-square'}"></i>
            <span>${title}</span>
            <button class="um-btn-icon ms-auto" @click=${() => this._closeModal()}><i class="bi bi-x-lg"></i></button>
          </div>
          <div class="um-modal-body">
            ${this._error ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:.85rem">${this._error}</div>` : nothing}

            ${mode === 'create' ? html`
              <div class="mb-3">
                <label class="form-label">${t('roles.form.id')} <span class="text-muted">${t('roles.form.id_hint')}</span></label>
                <input class="form-control font-monospace" placeholder=${t('roles.form.id_ph')} .value=${form.id}
                  @input=${e => this._patch('id', e.target.value)} />
                <div class="form-text" style="font-size:.75rem">${t('roles.form.id_desc')}</div>
              </div>
            ` : nothing}

            <div class="mb-3">
              <label class="form-label">${t('roles.form.label')}</label>
              <input class="form-control" .value=${form.label} @input=${e => this._patch('label', e.target.value)} />
            </div>
            <div class="mb-3">
              <label class="form-label">${t('roles.form.group')}</label>
              <select class="form-select" @change=${e => this._patch('permission_group', e.target.value)}>
                ${(this._groups ?? []).map(g => html`<option value=${g.id} ?selected=${form.permission_group === g.id}>${g.name}</option>`)}
              </select>
            </div>
            <div class="mb-3">
              <label class="form-label">${t('roles.form.interface')}</label>
              <select class="form-select" @change=${e => this._patch('ui_mode', e.target.value)}>
                <option value="full"   ?selected=${form.ui_mode === 'full'}>${t('roles.form.interface_full')}</option>
                <option value="simple" ?selected=${form.ui_mode === 'simple'}>${t('roles.form.interface_simple')}</option>
              </select>
              <div class="form-text" style="font-size:.75rem">${unsafeHTML(t('roles.form.interface_hint'))}</div>
            </div>
            <div class="mb-3">
              <label class="form-label">${t('roles.form.attrs')} <span class="text-muted">${t('roles.form.attrs_hint')}</span></label>
              <input class="form-control font-monospace" placeholder=${t('roles.form.attrs_ph')} .value=${form.attrs}
                @input=${e => this._patch('attrs', e.target.value)} />
            </div>
          </div>
          <div class="um-modal-footer">
            <button class="btn btn-sm btn-outline-secondary" @click=${() => this._closeModal()}>${t('roles.form.cancel')}</button>
            <button class="btn btn-sm btn-primary" @click=${() => this._save()}>
              <i class="bi bi-check-lg me-1"></i>${mode === 'create' ? t('roles.form.create') : t('roles.form.save')}
            </button>
          </div>
        </div>
      </div>
    `;
  }

  render() {
    if (!this._open) return nothing;
    const roles = this._roles ?? [];
    const loading = this._roles === null;

    return html`
      <div class="um-page">
        <div class="um-header">
          <h2 class="um-title"><i class="bi bi-tags me-2"></i>${t('roles.title')}</h2>
          <div class="um-header-right">
            <span class="um-header-count">${roles.length === 1 ? t('roles.count', { n: roles.length }) : t('roles.count_plural', { n: roles.length })}</span>
            <button class="btn btn-sm btn-primary" @click=${() => this._openCreate()}>
              <i class="bi bi-plus-lg me-1"></i>${t('roles.new_role')}
            </button>
          </div>
        </div>

        ${this._error && !this._modal ? html`
          <div class="alert alert-danger py-2 mx-4" style="font-size:.85rem">${this._error}</div>
        ` : nothing}

        <div class="um-table-wrap">
          ${loading ? html`<div class="um-empty"><i class="bi bi-hourglass-split"></i> ${t('roles.loading')}</div>` : roles.length === 0 ? html`
            <div class="um-empty"><i class="bi bi-tags"></i><p>${t('roles.empty')}</p></div>
          ` : html`
            <table class="um-table">
              <thead>
                <tr>
                  <th>${t('roles.col.id')}</th>
                  <th>${t('roles.col.label')}</th>
                  <th>${t('roles.col.group')}</th>
                  <th>${t('roles.col.interface')}</th>
                  <th></th>
                </tr>
              </thead>
              <tbody>
                ${roles.map(r => {
                  const isAdmin = r.id === ADMIN_ID;
                  return html`
                    <tr>
                      <td><code>${r.id}</code></td>
                      <td><strong>${r.label}</strong></td>
                      <td>${this._groupLabel(r.permission_group)}</td>
                      <td>${this._attrsUiMode(r.attrs) === 'simple'
                        ? html`<span class="badge" style="background:var(--accent-soft);color:var(--accent)">${t('roles.badge.simple')}</span>`
                        : html`<span class="badge bg-secondary">${t('roles.badge.full')}</span>`}</td>
                      <td>
                        <div class="um-actions">
                          <button class="um-btn-icon" title=${isAdmin ? t('roles.tooltip.locked') : t('roles.tooltip.edit')}
                            ?disabled=${isAdmin}
                            @click=${() => !isAdmin && this._openEdit(r)}>
                            <i class="bi bi-pencil"></i>
                          </button>
                          <button class="um-btn-icon" title=${isAdmin ? t('roles.tooltip.locked') : t('roles.tooltip.delete')}
                            ?disabled=${isAdmin}
                            @click=${() => !isAdmin && this._delete(r)}>
                            <i class="bi bi-trash"></i>
                          </button>
                        </div>
                      </td>
                    </tr>
                  `;
                })}
              </tbody>
            </table>
          `}
        </div>
      </div>
      ${this._renderModal()}
    `;
  }
}
