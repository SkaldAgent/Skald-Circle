import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';
import { t } from '../lib/i18n.js';

// Shared on-disk folders (blueprint §6). Admin-only surface: create a folder,
// describe what it holds (the description is fed to the assistant's system
// context), and grant members read-only or read-write access. There is no owner —
// a folder is just a name + a membership list (contrast: Projects, which will have
// an owner). Renaming is intentionally not offered (it would remount + move the
// on-disk directory). Reuses the `um-*` (users/roles) and `connector-card` styles.

async function jf(url, opts) {
  const res = await fetch(url, opts);
  if (!res.ok) throw new Error(await res.text() || `HTTP ${res.status}`);
  const ct = res.headers.get('content-type') || '';
  return ct.includes('application/json') ? res.json() : null;
}

export class SharedFoldersPage extends LightElement {

  static get properties() {
    return {
      _open:    { state: true },
      _folders: { state: true },   // [{ id, folder_name, description, members:[{user_id,can_write}] }]
      _users:   { state: true },   // /api/users — for the member picker + labels
      _error:   { state: true },
      _modal:   { state: true },   // null | { mode:'create'|'edit', folder?, form:{folder_name,description} }
      _add:     { state: true },   // { [folderId]: { user_id, can_write } } — in-progress add-row
    };
  }

  constructor() {
    super();
    this._open    = false;
    this._folders = null;
    this._users   = null;
    this._error   = null;
    this._modal   = null;
    this._add     = {};
  }

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'shared-folders';
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
      const [folders, users] = await Promise.all([
        jf('/api/shared-folders'),
        jf('/api/users'),
      ]);
      this._folders = folders;
      this._users   = users;
    } catch (e) {
      this._error   = e.message;
      this._folders = this._folders ?? [];
    }
  }

  _userLabel(id) {
    const u = (this._users ?? []).find(x => x.id === id);
    return u ? (u.display_name || u.username) : id;
  }

  // Users not yet members of this folder (and active) — the add-picker's options.
  _candidates(folder) {
    const members = new Set(folder.members.map(m => m.user_id));
    return (this._users ?? []).filter(u => u.active && !members.has(u.id));
  }

  // ── create / edit-description modal ──────────────────────────────────────────

  _openCreate() {
    this._modal = { mode: 'create', form: { folder_name: '', description: '' } };
    this._error = null;
  }

  _openEditDesc(folder) {
    this._modal = { mode: 'edit', folder, form: { folder_name: folder.folder_name, description: folder.description } };
    this._error = null;
  }

  _closeModal() { this._modal = null; this._error = null; }

  _patch(field, value) {
    this._modal = { ...this._modal, form: { ...this._modal.form, [field]: value } };
  }

  async _save() {
    const { mode, form, folder } = this._modal;
    this._error = null;
    try {
      if (mode === 'create') {
        if (!form.folder_name.trim()) { this._error = t('sf.error.name'); return; }
        await jf('/api/shared-folders', {
          method:  'POST',
          headers: { 'Content-Type': 'application/json' },
          body:    JSON.stringify({ folder_name: form.folder_name.trim(), description: form.description.trim() }),
        });
      } else {
        await jf(`/api/shared-folders/${folder.id}`, {
          method:  'PATCH',
          headers: { 'Content-Type': 'application/json' },
          body:    JSON.stringify({ description: form.description.trim() }),
        });
      }
      this._closeModal();
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  async _delete(folder) {
    if (!confirm(t('sf.confirm.delete', { name: folder.folder_name }))) return;
    this._error = null;
    try {
      await jf(`/api/shared-folders/${folder.id}`, { method: 'DELETE' });
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  // ── membership ───────────────────────────────────────────────────────────────

  _draft(folderId) { return this._add[folderId] ?? { user_id: '', can_write: false }; }

  _setAdd(folderId, patch) {
    this._add = { ...this._add, [folderId]: { ...this._draft(folderId), ...patch } };
  }

  async _addMember(folder) {
    const draft = this._draft(folder.id);
    if (!draft.user_id) return;
    this._error = null;
    try {
      await jf(`/api/shared-folders/${folder.id}/members`, {
        method:  'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({ user_id: draft.user_id, can_write: !!draft.can_write }),
      });
      this._add = { ...this._add, [folder.id]: { user_id: '', can_write: false } };
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  // Re-grant with a new capability — the POST upserts on (folder, user).
  async _setAccess(folder, userId, canWrite) {
    this._error = null;
    try {
      await jf(`/api/shared-folders/${folder.id}/members`, {
        method:  'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({ user_id: userId, can_write: canWrite }),
      });
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  async _removeMember(folder, userId) {
    if (!confirm(t('sf.confirm.remove_member', { name: this._userLabel(userId), folder: folder.folder_name }))) return;
    this._error = null;
    try {
      await jf(`/api/shared-folders/${folder.id}/members/${encodeURIComponent(userId)}`, { method: 'DELETE' });
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  // ── render ───────────────────────────────────────────────────────────────────

  render() {
    if (!this._open) return nothing;
    const folders = this._folders ?? [];
    const loading = this._folders === null;

    return html`
      <div class="um-page">
        <div class="um-header">
          <h2 class="um-title"><i class="bi bi-folder-symlink me-2"></i>${t('sf.title')}</h2>
          <div class="um-header-right">
            <span class="um-header-count">
              ${folders.length === 1 ? t('sf.count', { n: folders.length }) : t('sf.count_plural', { n: folders.length })}
            </span>
            <button class="btn btn-sm btn-primary" @click=${() => this._openCreate()}>
              <i class="bi bi-plus-lg me-1"></i>${t('sf.new')}
            </button>
          </div>
        </div>

        ${this._error && !this._modal ? html`
          <div class="alert alert-danger py-2 mx-4" style="font-size:.85rem">${this._error}</div>` : nothing}

        <div style="padding:0 1.25rem 1.5rem; overflow:auto">
          <div class="text-muted mb-3" style="font-size:.78rem">
            <i class="bi bi-info-circle me-1"></i>${t('sf.note.propagation')}
          </div>
          ${loading
            ? html`<div class="um-empty"><i class="bi bi-hourglass-split"></i> ${t('sf.loading')}</div>`
            : folders.length === 0
              ? html`
                <div class="um-empty">
                  <i class="bi bi-folder-symlink"></i>
                  <p>${t('sf.empty')}</p>
                  <p style="font-size:.8rem;opacity:.7">${t('sf.empty_hint')}</p>
                </div>`
              : html`<div class="d-flex flex-column gap-3">${folders.map(f => this._renderFolder(f))}</div>`}
        </div>

        ${this._renderModal()}
      </div>`;
  }

  _renderFolder(f) {
    const draft      = this._draft(f.id);
    const candidates = this._candidates(f);

    return html`
      <div class="connector-card" style="cursor:default">
        <div class="d-flex align-items-start justify-content-between">
          <div style="min-width:0">
            <div style="font-weight:600;font-size:.95rem">
              <i class="bi bi-folder2 me-1" style="opacity:.6"></i>${f.folder_name}
            </div>
            <code class="text-muted" style="font-size:.7rem">shared/${f.folder_name}</code>
          </div>
          <div class="d-flex gap-1">
            <button class="btn btn-sm btn-outline-secondary" title=${t('sf.edit_desc')} @click=${() => this._openEditDesc(f)}>
              <i class="bi bi-pencil"></i>
            </button>
            <button class="btn btn-sm btn-outline-danger" title=${t('sf.delete')} @click=${() => this._delete(f)}>
              <i class="bi bi-trash"></i>
            </button>
          </div>
        </div>

        <div class="mt-2 mb-3" style="font-size:.82rem">
          ${f.description
            ? html`<span>${f.description}</span>`
            : html`<span class="text-muted fst-italic">${t('sf.no_desc')}</span>`}
        </div>

        <div style="border-top:1px solid var(--bs-border-color,#333);padding-top:.75rem">
          <div class="text-muted mb-2" style="font-size:.72rem;text-transform:uppercase;letter-spacing:.04em">
            ${t('sf.members')}
          </div>

          ${f.members.length === 0
            ? html`<div class="text-muted mb-2" style="font-size:.8rem">${t('sf.no_members')}</div>`
            : html`<div class="d-flex flex-column gap-1 mb-2">${f.members.map(m => this._renderMember(f, m))}</div>`}

          ${candidates.length > 0 ? html`
            <div class="d-flex gap-2 align-items-center flex-wrap">
              <select class="form-select form-select-sm" style="max-width:16rem"
                @change=${(e) => this._setAdd(f.id, { user_id: e.target.value })}>
                <option value="" ?selected=${!draft.user_id}>${t('sf.choose_user')}</option>
                ${candidates.map(u => html`
                  <option value=${u.id} ?selected=${draft.user_id === u.id}>${u.display_name || u.username}</option>`)}
              </select>
              <select class="form-select form-select-sm" style="max-width:11rem"
                @change=${(e) => this._setAdd(f.id, { can_write: e.target.value === 'write' })}>
                <option value="read"  ?selected=${!draft.can_write}>${t('sf.access.readonly')}</option>
                <option value="write" ?selected=${draft.can_write}>${t('sf.access.readwrite')}</option>
              </select>
              <button class="btn btn-sm btn-primary" ?disabled=${!draft.user_id} @click=${() => this._addMember(f)}>
                <i class="bi bi-plus-lg me-1"></i>${t('sf.add')}
              </button>
            </div>`
            : html`<div class="text-muted" style="font-size:.78rem">${t('sf.all_added')}</div>`}
        </div>
      </div>`;
  }

  _renderMember(f, m) {
    return html`
      <div class="d-flex align-items-center justify-content-between p-2 rounded"
           style="border:1px solid var(--bs-border-color,#333)">
        <div style="min-width:0;font-size:.85rem">
          <i class="bi bi-person-circle me-1" style="opacity:.6"></i>${this._userLabel(m.user_id)}
        </div>
        <div class="d-flex align-items-center gap-2">
          <div class="btn-group btn-group-sm" role="group" aria-label=${t('sf.access.label')}>
            <button class="btn ${!m.can_write ? 'btn-secondary' : 'btn-outline-secondary'}"
              title=${t('sf.access.readonly')}
              @click=${() => m.can_write && this._setAccess(f, m.user_id, false)}>
              <i class="bi bi-eye me-1"></i>${t('sf.access.read')}
            </button>
            <button class="btn ${m.can_write ? 'btn-secondary' : 'btn-outline-secondary'}"
              title=${t('sf.access.readwrite')}
              @click=${() => !m.can_write && this._setAccess(f, m.user_id, true)}>
              <i class="bi bi-pencil me-1"></i>${t('sf.access.write')}
            </button>
          </div>
          <button class="um-btn-icon" title=${t('sf.remove')} @click=${() => this._removeMember(f, m.user_id)}>
            <i class="bi bi-x-lg"></i>
          </button>
        </div>
      </div>`;
  }

  _renderModal() {
    if (!this._modal) return nothing;
    const { mode, form, folder } = this._modal;
    const title = mode === 'create' ? t('sf.form.new') : t('sf.form.edit', { name: folder.folder_name });

    return html`
      <div class="um-modal-overlay" @click=${(e) => { if (e.target.classList.contains('um-modal-overlay')) this._closeModal(); }}>
        <div class="um-modal">
          <div class="um-modal-header">
            <i class="bi ${mode === 'create' ? 'bi-folder-plus' : 'bi-pencil-square'}"></i>
            <span>${title}</span>
            <button class="um-btn-icon ms-auto" @click=${() => this._closeModal()}><i class="bi bi-x-lg"></i></button>
          </div>
          <div class="um-modal-body">
            ${this._error ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:.85rem">${this._error}</div>` : nothing}

            ${mode === 'create' ? html`
              <div class="mb-3">
                <label class="form-label">${t('sf.form.name')} <span class="text-muted">${t('sf.form.name_hint')}</span></label>
                <input class="form-control font-monospace" placeholder=${t('sf.form.name_ph')} .value=${form.folder_name}
                  @input=${e => this._patch('folder_name', e.target.value)} />
                <div class="form-text" style="font-size:.75rem">${t('sf.form.name_desc')}</div>
              </div>
            ` : html`
              <div class="mb-3">
                <label class="form-label">${t('sf.form.name')}</label>
                <div><code>shared/${folder.folder_name}</code></div>
              </div>
            `}

            <div class="mb-3">
              <label class="form-label">${t('sf.form.desc')}</label>
              <textarea class="form-control" rows="4" placeholder=${t('sf.form.desc_ph')} .value=${form.description}
                @input=${e => this._patch('description', e.target.value)}></textarea>
              <div class="form-text" style="font-size:.75rem">${t('sf.form.desc_desc')}</div>
            </div>
          </div>
          <div class="um-modal-footer">
            <button class="btn btn-sm btn-outline-secondary" @click=${() => this._closeModal()}>${t('sf.form.cancel')}</button>
            <button class="btn btn-sm btn-primary" @click=${() => this._save()}>
              <i class="bi bi-check-lg me-1"></i>${mode === 'create' ? t('sf.form.create') : t('sf.form.save')}
            </button>
          </div>
        </div>
      </div>`;
  }
}
