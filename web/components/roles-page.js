import { html, nothing } from 'lit';
import { unsafeHTML }     from 'lit/directives/unsafe-html.js';
import { LightElement } from '../lib/base.js';
import { t }            from '../lib/i18n.js';

const ADMIN_ID = 'admin';
// Source-driven chat agent (needs a project run-context): never a personal default,
// so it is excluded from the role assistant picker (mirrors the server-side guard).
const PROJECT_COORDINATOR_ID = 'project-coordinator';

export class RolesPage extends LightElement {

  static get properties() {
    return {
      _open:    { state: true },
      _roles:   { state: true },
      _groups:  { state: true },
      _agents:  { state: true },
      _error:   { state: true },
      _modal:   { state: true },  // null | { mode: 'create'|'edit', role?, form }
    };
  }

  constructor() {
    super();
    this._open   = false;
    this._roles  = null;
    this._groups = null;
    this._agents = null;
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
      const [rRes, gRes, aRes] = await Promise.all([
        fetch('/api/roles'),
        fetch('/api/tool-permission-groups'),
        fetch('/api/agents'),
      ]);
      if (!rRes.ok) throw new Error(`HTTP ${rRes.status}`);
      if (!gRes.ok) throw new Error(`HTTP ${gRes.status}`);
      if (!aRes.ok) throw new Error(`HTTP ${aRes.status}`);
      this._roles  = await rRes.json();
      this._groups = await gRes.json();
      // Only `type:chat` agents are entry agents; project-coordinator is source-driven.
      this._agents = (await aRes.json())
        .filter(a => a.type === 'chat' && a.id !== PROJECT_COORDINATOR_ID);
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

  // Extra security-groups the role may pick beyond its default `permission_group`
  // (the effective set is default ∪ these). Lives in attrs JSON (§0.1).
  _attrsAllowedGroups(attrs) {
    try {
      const a = JSON.parse(attrs || '{}').permission_groups;
      return Array.isArray(a) ? a : [];
    } catch { return []; }
  }

  // The role's default entry agent (data-driven, §0.1). Empty means "fall back to the
  // instance default assistant" — the server resolver handles it, so we store nothing.
  _attrsChatAgent(attrs) {
    try { const a = JSON.parse(attrs || '{}').chat_agent; return typeof a === 'string' ? a : ''; }
    catch { return ''; }
  }

  // Display name for a chat-agent id (falls back to the id, or the default label when unset).
  _agentName(id) {
    if (!id) return t('roles.form.assistant_default');
    return this._agents?.find(a => a.id === id)?.name ?? id;
  }

  // Whether a plugin or connector the admin installs reaches this role on its own.
  // Absent means yes — the server's RoleAttrs defaults it to true, so only an
  // opt-out is ever written (see `db::access_defaults`).
  _attrsAutoGrant(attrs) {
    try { return JSON.parse(attrs || '{}').auto_grant !== false; }
    catch { return true; }
  }

  _mergeAttrs(attrs, uiMode, allowedGroups, chatAgent, autoGrant) {
    let o = {};
    try { o = JSON.parse(attrs || '{}') ?? {}; } catch { o = {}; }
    if (uiMode === 'simple') o.ui_mode = 'simple'; else delete o.ui_mode;
    const extras = Array.isArray(allowedGroups) ? allowedGroups.filter(Boolean) : [];
    if (extras.length) o.permission_groups = extras; else delete o.permission_groups;
    if (chatAgent) o.chat_agent = chatAgent; else delete o.chat_agent;
    // Only the opt-out is persisted; `true` is the server-side default.
    if (autoGrant === false) o.auto_grant = false; else delete o.auto_grant;
    const keys = Object.keys(o);
    return keys.length ? JSON.stringify(o) : null;
  }

  _openCreate() {
    this._modal = {
      mode: 'create',
      form: { id: '', label: '', permission_group: this._groups?.[0]?.id ?? 'default', attrs: '', ui_mode: 'full', allowed_groups: [], chat_agent: '', auto_grant: true },
    };
  }

  _openEdit(role) {
    this._modal = {
      mode: 'edit',
      role,
      form: { label: role.label, permission_group: role.permission_group, attrs: role.attrs ?? '', ui_mode: this._attrsUiMode(role.attrs), allowed_groups: this._attrsAllowedGroups(role.attrs), chat_agent: this._attrsChatAgent(role.attrs), auto_grant: this._attrsAutoGrant(role.attrs) },
    };
  }

  _closeModal() { this._modal = null; this._error = null; }

  _patch(field, value) {
    this._modal = { ...this._modal, form: { ...this._modal.form, [field]: value } };
  }

  _toggleAllowedGroup(id, checked) {
    const cur = new Set(this._modal.form.allowed_groups || []);
    if (checked) cur.add(id); else cur.delete(id);
    this._patch('allowed_groups', [...cur]);
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
            attrs: this._mergeAttrs(form.attrs, form.ui_mode, form.allowed_groups, form.chat_agent, form.auto_grant),
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
            attrs: this._mergeAttrs(form.attrs, form.ui_mode, form.allowed_groups, form.chat_agent, form.auto_grant),
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
              <label class="form-label">${t('roles.form.allowed')}</label>
              <div class="form-text mb-2" style="font-size:.75rem">${t('roles.form.allowed_hint')}</div>
              ${(this._groups ?? []).filter(g => g.id !== form.permission_group).map(g => html`
                <div class="form-check">
                  <input class="form-check-input" type="checkbox" id="allow-${g.id}"
                    .checked=${(form.allowed_groups || []).includes(g.id)}
                    @change=${e => this._toggleAllowedGroup(g.id, e.target.checked)} />
                  <label class="form-check-label" for="allow-${g.id}">${g.name}</label>
                </div>
              `)}
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
              <label class="form-label">${t('roles.form.assistant')}</label>
              <select class="form-select" @change=${e => this._patch('chat_agent', e.target.value)}>
                <option value="" ?selected=${!form.chat_agent}>${t('roles.form.assistant_default')}</option>
                ${(this._agents ?? []).map(a => html`<option value=${a.id} ?selected=${form.chat_agent === a.id}>${a.name}</option>`)}
              </select>
              <div class="form-text" style="font-size:.75rem">${t('roles.form.assistant_hint')}</div>
            </div>
            <div class="mb-3">
              <label class="form-label">${t('roles.form.auto_grant')}</label>
              <div class="form-check">
                <input class="form-check-input" type="checkbox" id="role-auto-grant"
                  .checked=${form.auto_grant !== false}
                  @change=${e => this._patch('auto_grant', e.target.checked)} />
                <label class="form-check-label" for="role-auto-grant">${t('roles.form.auto_grant_label')}</label>
              </div>
              <div class="form-text" style="font-size:.75rem">${t('roles.form.auto_grant_hint')}</div>
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
        <div class="page-header">
          <div class="page-header-left">
            <h2 class="page-header-title"><i class="bi bi-tags me-2"></i>${t('roles.title')}</h2>
          </div>
          <div class="page-header-actions">
            <span class="page-header-count">${roles.length === 1 ? t('roles.count', { n: roles.length }) : t('roles.count_plural', { n: roles.length })}</span>
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
                  <th>${t('roles.col.assistant')}</th>
                  <th>${t('roles.col.auto_grant')}</th>
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
                      <td>${this._agentName(this._attrsChatAgent(r.attrs))}</td>
                      <td>${isAdmin || this._attrsAutoGrant(r.attrs)
                        ? html`<span class="badge bg-secondary">${t('roles.badge.auto_grant_on')}</span>`
                        : html`<span class="badge" style="background:var(--accent-soft);color:var(--accent)">${t('roles.badge.auto_grant_off')}</span>`}</td>
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
