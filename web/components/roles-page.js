import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';

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
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'roles';
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) this._load();
    });
  }

  async _load() {
    this._error = null;
    try {
      const [rRes, gRes] = await Promise.all([
        fetch('/api/roles'),
        fetch('/api/tool-permission-groups'),
      ]);
      if (!rRes.ok) throw new Error(`Roles: HTTP ${rRes.status}`);
      if (!gRes.ok) throw new Error(`Groups: HTTP ${gRes.status}`);
      this._roles  = await rRes.json();
      this._groups = await gRes.json();
    } catch (e) {
      this._error = e.message;
    }
  }

  // ── Modal helpers ────────────────────────────────────────────────────────────

  _openCreate() {
    this._modal = {
      mode: 'create',
      form: { id: '', label: '', permission_group: this._groups?.[0]?.id ?? 'default', attrs: '' },
    };
  }

  _openEdit(role) {
    this._modal = {
      mode: 'edit',
      role,
      form: { label: role.label, permission_group: role.permission_group, attrs: role.attrs ?? '' },
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
      if (!form.id.trim() || !form.label.trim()) { this._error = 'ID and label are required.'; return; }
      try {
        const res = await fetch('/api/roles', {
          method: 'POST',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            id: form.id.trim(),
            label: form.label.trim(),
            permission_group: form.permission_group,
            attrs: form.attrs.trim() || null,
          }),
        });
        if (!res.ok) throw new Error(await res.text());
        this._closeModal();
        await this._load();
      } catch (e) { this._error = e.message; }
    } else {
      const { role } = this._modal;
      if (!form.label.trim()) { this._error = 'Label is required.'; return; }
      try {
        const res = await fetch(`/api/roles/${role.id}`, {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({
            label: form.label.trim(),
            permission_group: form.permission_group,
            attrs: form.attrs.trim() || null,
          }),
        });
        if (!res.ok) throw new Error(await res.text());
        this._closeModal();
        await this._load();
      } catch (e) { this._error = e.message; }
    }
  }

  async _delete(role) {
    if (!confirm(`Delete role "${role.label}"?`)) return;
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
    const title = mode === 'create' ? 'New role' : `Edit ${role.label}`;

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
                <label class="form-label">ID <span class="text-muted">(slug)</span></label>
                <input class="form-control font-monospace" placeholder="e.g. editor" .value=${form.id}
                  @input=${e => this._patch('id', e.target.value)} />
                <div class="form-text" style="font-size:.75rem">Lowercase, no spaces. Cannot be changed later.</div>
              </div>
            ` : nothing}

            <div class="mb-3">
              <label class="form-label">Label</label>
              <input class="form-control" .value=${form.label} @input=${e => this._patch('label', e.target.value)} />
            </div>
            <div class="mb-3">
              <label class="form-label">Permission group</label>
              <select class="form-select" @change=${e => this._patch('permission_group', e.target.value)}>
                ${(this._groups ?? []).map(g => html`<option value=${g.id} ?selected=${form.permission_group === g.id}>${g.name}</option>`)}
              </select>
            </div>
            <div class="mb-3">
              <label class="form-label">Attrs <span class="text-muted">(JSON, optional)</span></label>
              <input class="form-control font-monospace" placeholder="{}" .value=${form.attrs}
                @input=${e => this._patch('attrs', e.target.value)} />
            </div>
          </div>
          <div class="um-modal-footer">
            <button class="btn btn-sm btn-outline-secondary" @click=${() => this._closeModal()}>Cancel</button>
            <button class="btn btn-sm btn-primary" @click=${() => this._save()}>
              <i class="bi bi-check-lg me-1"></i>${mode === 'create' ? 'Create' : 'Save'}
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
          <h2 class="um-title"><i class="bi bi-tags me-2"></i>Roles</h2>
          <div class="um-header-right">
            <span class="um-header-count">${roles.length} role${roles.length === 1 ? '' : 's'}</span>
            <button class="btn btn-sm btn-primary" @click=${() => this._openCreate()}>
              <i class="bi bi-plus-lg me-1"></i>New role
            </button>
          </div>
        </div>

        ${this._error && !this._modal ? html`
          <div class="alert alert-danger py-2 mx-4" style="font-size:.85rem">${this._error}</div>
        ` : nothing}

        <div class="um-table-wrap">
          ${loading ? html`<div class="um-empty"><i class="bi bi-hourglass-split"></i> Loading…</div>` : roles.length === 0 ? html`
            <div class="um-empty"><i class="bi bi-tags"></i><p>No roles.</p></div>
          ` : html`
            <table class="um-table">
              <thead>
                <tr>
                  <th>ID</th>
                  <th>Label</th>
                  <th>Permission group</th>
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
                      <td>
                        <div class="um-actions">
                          <button class="um-btn-icon" title=${isAdmin ? 'Built-in role — locked' : 'Edit'}
                            ?disabled=${isAdmin}
                            @click=${() => !isAdmin && this._openEdit(r)}>
                            <i class="bi bi-pencil"></i>
                          </button>
                          <button class="um-btn-icon" title=${isAdmin ? 'Built-in role — locked' : 'Delete'}
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
