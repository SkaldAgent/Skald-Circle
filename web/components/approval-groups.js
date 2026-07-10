import { html, nothing } from 'lit';
import { LightElement }  from '../lib/base.js';

export class ApprovalGroupsPage extends LightElement {
  static properties = {
    _open:          { state: true },
    _groups:        { state: true },
    _rules:         { state: true },
    _error:         { state: true },
    _groupEditId:   { state: true },
    _groupForm:     { state: true },
    _groupSaving:   { state: true },
    _duplicateOf:   { state: true },   // group being duplicated, or null
    _dupForm:       { state: true },   // { id, name }
    _dupSaving:     { state: true },
  };

  constructor() {
    super();
    this._open          = false;
    this._groups        = [];
    this._rules         = [];
    this._error         = null;
    this._groupEditId   = null;
    this._groupForm     = { id: '', name: '', description: '' };
    this._groupSaving   = false;
    this._duplicateOf   = null;
    this._dupForm       = { id: '', name: '' };
    this._dupSaving     = false;
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('llm-page-change', async (e) => {
      this._open = e.detail.page === 'approval';
      this.style.display = this._open ? 'flex' : 'none';
      if (!this._open) return;
      await this._load();
      const match = window.location.hash.match(/^#approval\/(.+)$/);
      if (match) {
        const group = this._groups.find(g => g.id === decodeURIComponent(match[1]));
        if (group) { this._navigateTo(group); return; }
      }
    });
    window.addEventListener('approval-navigate', (e) => {
      if (e.detail.group !== null) return;
      // Returning from rules view — show groups again
      this._open = true;
      this.style.display = 'flex';
      this._load();
    });
    window.addEventListener('hashchange', () => {
      if (!this._open) return;
      const match = window.location.hash.match(/^#approval\/(.+)$/);
      if (!match) return;
      const group = this._groups.find(g => g.id === decodeURIComponent(match[1]));
      if (group) this._navigateTo(group);
    });
  }

  async _load() {
    this._error = null;
    try {
      const [gRes, rRes] = await Promise.all([
        fetch('/api/tool-permission-groups'),
        fetch('/api/approval/rules'),
      ]);
      if (!gRes.ok) throw new Error(`Groups: HTTP ${gRes.status}`);
      if (!rRes.ok) throw new Error(`Rules: HTTP ${rRes.status}`);
      const groups = await gRes.json();
      this._groups = groups.sort((a, b) => {
        if (a.id === 'default') return -1;
        if (b.id === 'default') return 1;
        return a.name.localeCompare(b.name);
      });
      this._rules = await rRes.json();
    } catch (e) {
      this._error = e.message;
    }
  }

  _rulesForGroup(groupId) {
    return this._rules.filter(r => (r.group_id ?? 'default') === groupId);
  }

  // ── Navigation ────────────────────────────────────────────────────────────────

  _navigateTo(group) {
    this._open = false;
    this.style.display = 'none';
    window.location.hash = `approval/${group.id}`;
    window.dispatchEvent(new CustomEvent('approval-navigate', { detail: { group } }));
  }

  // ── Group management ──────────────────────────────────────────────────────────

  _startNewGroup() {
    this._groupEditId = 'new';
    this._groupForm   = { id: '', name: '', description: '' };
    this._duplicateOf = null;
  }

  _startEditGroup(group) {
    this._groupEditId = group.id;
    this._groupForm   = { id: group.id, name: group.name, description: group.description ?? '' };
    this._duplicateOf = null;
  }

  _cancelGroupEdit() { this._groupEditId = null; }

  _patchGroup(field, value) {
    this._groupForm = { ...this._groupForm, [field]: value };
  }

  async _saveGroup() {
    const isNew = this._groupEditId === 'new';
    if (!this._groupForm.name.trim()) { this._error = 'Group name is required.'; return; }
    if (isNew && !this._groupForm.id.trim()) { this._error = 'Group ID is required.'; return; }
    this._groupSaving = true;
    this._error = null;
    try {
      const body = isNew
        ? { id: this._groupForm.id.trim(), name: this._groupForm.name.trim(), description: this._groupForm.description.trim() || null }
        : { name: this._groupForm.name.trim(), description: this._groupForm.description.trim() || null };
      const url = isNew ? '/api/tool-permission-groups' : `/api/tool-permission-groups/${this._groupEditId}`;
      const res = await fetch(url, {
        method:  isNew ? 'POST' : 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify(body),
      });
      if (!res.ok) throw new Error(await res.text());
      this._groupEditId = null;
      await this._load();
    } catch (e) {
      this._error = e.message;
    } finally {
      this._groupSaving = false;
    }
  }

  async _deleteGroup(group) {
    const count = this._rulesForGroup(group.id).length;
    const msg = count > 0
      ? `Delete group "${group.name}" and its ${count} rule${count === 1 ? '' : 's'}?`
      : `Delete group "${group.name}"?`;
    if (!confirm(msg)) return;
    try {
      const res = await fetch(`/api/tool-permission-groups/${group.id}`, { method: 'DELETE' });
      if (!res.ok) throw new Error(await res.text());
      await this._load();
    } catch (e) {
      this._error = e.message;
    }
  }

  // ── Duplicate group ───────────────────────────────────────────────────────────

  _startDuplicate(group) {
    this._duplicateOf = group;
    this._dupForm     = {
      id:   `${group.id}_copy`,
      name: `Copy of ${group.name}`,
    };
    this._groupEditId = null; // close any open create/rename form
  }

  _cancelDuplicate() { this._duplicateOf = null; }

  async _saveDuplicate() {
    if (!this._dupForm.name.trim()) { this._error = 'Name is required.'; return; }
    if (!this._dupForm.id.trim())   { this._error = 'ID is required.';   return; }
    this._dupSaving = true;
    this._error     = null;
    try {
      const res = await fetch(`/api/tool-permission-groups/${this._duplicateOf.id}/duplicate`, {
        method:  'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({ id: this._dupForm.id.trim(), name: this._dupForm.name.trim() }),
      });
      if (!res.ok) throw new Error(await res.text());
      this._duplicateOf = null;
      await this._load();
    } catch (e) {
      this._error = e.message;
    } finally {
      this._dupSaving = false;
    }
  }

  // ── Group form ────────────────────────────────────────────────────────────────

  _renderGroupForm() {
    const isNew = this._groupEditId === 'new';
    const f     = this._groupForm;
    return html`
      <div class="apr-form">
        <div class="apr-form-header">
          <i class="bi bi-collection"></i>
          <span>${isNew ? 'New group' : 'Rename group'}</span>
          <button class="apr-form-close" @click=${() => this._cancelGroupEdit()}>
            <i class="bi bi-x"></i>
          </button>
        </div>
        <div class="apr-form-body">
          <div class="row g-3">
            ${isNew ? html`
              <div class="col-12">
                <label class="form-label fw-semibold" style="font-size:0.82rem">ID <span class="text-danger">*</span></label>
                <input
                  class="form-control form-control-sm font-monospace"
                  placeholder="e.g. cron_strict"
                  .value=${f.id}
                  @input=${(e) => this._patchGroup('id', e.target.value)}
                />
                <div class="form-text" style="font-size:0.75rem">Lowercase slug, no spaces. Cannot be changed later.</div>
              </div>
            ` : nothing}
            <div class="col-12">
              <label class="form-label fw-semibold" style="font-size:0.82rem">Name <span class="text-danger">*</span></label>
              <input
                class="form-control form-control-sm"
                placeholder="e.g. Cron strict"
                .value=${f.name}
                @input=${(e) => this._patchGroup('name', e.target.value)}
              />
            </div>
            <div class="col-12">
              <label class="form-label fw-semibold" style="font-size:0.82rem">Description <span class="text-muted fw-normal">(optional)</span></label>
              <input
                class="form-control form-control-sm"
                placeholder="Short description…"
                .value=${f.description}
                @input=${(e) => this._patchGroup('description', e.target.value)}
              />
            </div>
          </div>
          <div class="apr-form-actions">
            <button type="button" class="btn btn-sm btn-outline-secondary" @click=${() => this._cancelGroupEdit()}>Cancel</button>
            <button class="btn btn-sm btn-primary" @click=${() => this._saveGroup()} ?disabled=${this._groupSaving}>
              ${this._groupSaving
                ? html`<span class="spinner-border spinner-border-sm me-1"></span>Saving…`
                : html`<i class="bi bi-check-lg me-1"></i>Save`}
            </button>
          </div>
        </div>
      </div>
    `;
  }

  // ── Duplicate form ────────────────────────────────────────────────────────────

  _renderDuplicateForm() {
    const src = this._duplicateOf;
    const f   = this._dupForm;
    return html`
      <div class="apr-form">
        <div class="apr-form-header">
          <i class="bi bi-copy"></i>
          <span>Duplicate <strong>${src.name}</strong></span>
          <button class="apr-form-close" @click=${() => this._cancelDuplicate()}>
            <i class="bi bi-x"></i>
          </button>
        </div>
        <div class="apr-form-body">
          <div class="row g-3">
            <div class="col-12">
              <label class="form-label fw-semibold" style="font-size:0.82rem">New name <span class="text-danger">*</span></label>
              <input
                class="form-control form-control-sm"
                .value=${f.name}
                @input=${(e) => { this._dupForm = { ...this._dupForm, name: e.target.value }; }}
              />
            </div>
            <div class="col-12">
              <label class="form-label fw-semibold" style="font-size:0.82rem">New ID <span class="text-danger">*</span></label>
              <input
                class="form-control form-control-sm font-monospace"
                .value=${f.id}
                @input=${(e) => { this._dupForm = { ...this._dupForm, id: e.target.value }; }}
              />
              <div class="form-text" style="font-size:0.75rem">Lowercase slug, no spaces. Cannot be changed later.</div>
            </div>
          </div>
          <div class="apr-form-body" style="padding:0;margin-top:0.5rem">
            <div class="alert alert-info py-2 mb-0" style="font-size:0.8rem">
              <i class="bi bi-info-circle me-1"></i>
              All <strong>${this._rulesForGroup(src.id).length}</strong> rule${this._rulesForGroup(src.id).length === 1 ? '' : 's'} from <em>${src.name}</em> will be copied.
            </div>
          </div>
          <div class="apr-form-actions">
            <button type="button" class="btn btn-sm btn-outline-secondary" @click=${() => this._cancelDuplicate()}>Cancel</button>
            <button class="btn btn-sm btn-primary" @click=${() => this._saveDuplicate()} ?disabled=${this._dupSaving}>
              ${this._dupSaving
                ? html`<span class="spinner-border spinner-border-sm me-1"></span>Duplicating…`
                : html`<i class="bi bi-copy me-1"></i>Duplicate`}
            </button>
          </div>
        </div>
      </div>
    `;
  }

  // ── Group card ────────────────────────────────────────────────────────────────

  _renderGroupCard(group) {
    const count     = this._rulesForGroup(group.id).length;
    const isDefault = group.id === 'default';
    return html`
      <div class="apr-card apr-group-card" @click=${() => this._navigateTo(group)}>
        <div class="apr-card-row1">
          ${isDefault ? html`<span class="apr-group-default-badge">Default</span>` : nothing}
          <span class="apr-group-name">${group.name}</span>
          <span class="apr-priority-badge ms-auto" title="${count} rule${count === 1 ? '' : 's'}">
            <i class="bi bi-list-ul"></i>
            ${count}
          </span>
          <div class="apr-card-actions" @click=${(e) => e.stopPropagation()}>
            <button class="apr-btn-icon" title="Duplicate"
              @click=${(e) => { e.stopPropagation(); this._startDuplicate(group); }}>
              <i class="bi bi-copy"></i>
            </button>
            <button class="apr-btn-icon apr-btn-edit" title="Rename"
              @click=${(e) => { e.stopPropagation(); this._startEditGroup(group); }}>
              <i class="bi bi-pencil"></i>
            </button>
            <button
              class="apr-btn-icon apr-btn-delete"
              title=${isDefault ? 'Cannot delete the default group' : 'Delete group'}
              ?disabled=${isDefault}
              @click=${(e) => { e.stopPropagation(); if (!isDefault) this._deleteGroup(group); }}
            >
              <i class="bi bi-trash"></i>
            </button>
          </div>
        </div>
        ${group.description ? html`
          <div class="apr-card-row3">
            <span class="apr-tag"><i class="bi bi-text-left"></i>${group.description}</span>
          </div>
        ` : nothing}
      </div>
    `;
  }

  // ── Groups view ───────────────────────────────────────────────────────────────

  render() {
    return html`
      <div class="apr-page">
        <div class="apr-header">
          <h2 class="apr-title">
            <i class="bi bi-shield-check me-2"></i>Security
          </h2>
          <div class="apr-header-right">
            <span class="apr-header-count">${this._groups.length} group${this._groups.length === 1 ? '' : 's'}</span>
            <button class="btn btn-sm btn-primary" @click=${() => this._startNewGroup()}>
              <i class="bi bi-plus-lg me-1"></i>New group
            </button>
          </div>
        </div>

        <div class="agent-info-banner" style="margin: 14px 20px 0">
          <div class="agent-info-banner-icon"><i class="bi bi-info-circle-fill"></i></div>
          <div class="agent-info-banner-body">
            <p class="mb-1">
              <strong>Permission groups</strong> are named sets of approval rules.
              A session's active <strong>Agent Profile</strong> determines which group applies —
              that group's rules are evaluated first, with the <strong>Default</strong> group as fallback.
            </p>
            <p class="mb-0">
              Click a group to view and manage its rules.
              The <strong>Default</strong> group cannot be deleted, but its rules can be edited freely.
            </p>
          </div>
        </div>

        ${this._error ? html`
          <div class="alert alert-danger py-2 mx-3 mt-3 mb-0" style="font-size:0.85rem">${this._error}</div>
        ` : nothing}

        ${this._groupEditId !== null ? this._renderGroupForm() : nothing}
        ${this._duplicateOf  !== null ? this._renderDuplicateForm() : nothing}

        <div class="apr-card-list">
          ${this._groups.length === 0 ? html`
            <div class="apr-empty">
              <i class="bi bi-collection"></i>
              <p>No groups yet.</p>
              <button class="btn btn-sm btn-primary" @click=${() => this._startNewGroup()}>
                <i class="bi bi-plus-lg me-1"></i>Create first group
              </button>
            </div>
          ` : this._groups.map(g => this._renderGroupCard(g))}
        </div>
      </div>
    `;
  }
}
