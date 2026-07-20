import { html, nothing } from 'lit';
import { LightElement } from '../../lib/base.js';
import { t }            from '../../lib/i18n.js';

/// A project's detail page: header + description, a sharing panel (member picker with
/// read/write, mirroring the shared-folders UI), Open chat, and a Files section (the
/// future primary surface — a file explorer over the project folder). No ticket board.
export class ProjectBoardSection extends LightElement {
  static properties = {
    _project:  { state: true },
    _users:    { state: true },
    _add:      { state: true },
    _error:    { state: true },
  };

  constructor() {
    super();
    this._project   = null;
    this._users     = [];
    this._add       = { user_id: '', can_write: false };
    this._error     = null;
    this._projectId = null;
  }

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
  }

  disconnectedCallback() {
    window.removeEventListener('locale-changed', this.__onLocaleChanged);
    super.disconnectedCallback();
  }

  async load(projectId) {
    this._projectId = projectId;
    this._project   = null;
    this._error     = null;
    try {
      const [projRes, usersRes] = await Promise.all([
        fetch(`/api/projects/${projectId}`),
        fetch('/api/users'),
      ]);
      if (!projRes.ok) throw new Error(`HTTP ${projRes.status}`);
      this._project = await projRes.json();
      if (usersRes.ok) this._users = await usersRes.json();
    } catch (e) {
      this._error = e.message;
    }
  }

  async _reload() {
    try {
      const res = await fetch(`/api/projects/${this._projectId}`);
      if (res.ok) this._project = await res.json();
    } catch { /* transient */ }
  }

  _canManage() {
    return !!this._project && (this._project.is_owner || this._project.can_write);
  }

  _userLabel(id) {
    const u = this._users.find(u => u.id === id);
    return u ? (u.display_name || u.username) : id;
  }

  _candidates() {
    const taken = new Set((this._project?.members ?? []).map(m => m.user_id));
    return this._users.filter(u => u.active !== false && !taken.has(u.id));
  }

  // ── Membership actions ─────────────────────────────────────────────────────────

  async _addMember() {
    if (!this._add.user_id) return;
    try {
      const res = await fetch(`/api/projects/${this._projectId}/members`, {
        method:  'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({ user_id: this._add.user_id, can_write: this._add.can_write }),
      });
      if (!res.ok) throw new Error(await res.text());
      this._project = { ...this._project, members: await res.json() };
      this._add = { user_id: '', can_write: false };
    } catch (e) {
      this._error = e.message;
    }
  }

  async _setAccess(userId, canWrite) {
    try {
      const res = await fetch(`/api/projects/${this._projectId}/members`, {
        method:  'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({ user_id: userId, can_write: canWrite }),
      });
      if (!res.ok) throw new Error(await res.text());
      this._project = { ...this._project, members: await res.json() };
    } catch (e) {
      this._error = e.message;
    }
  }

  async _removeMember(userId) {
    try {
      const res = await fetch(`/api/projects/${this._projectId}/members/${encodeURIComponent(userId)}`,
        { method: 'DELETE' });
      if (!res.ok) throw new Error(await res.text());
      this._project = { ...this._project, members: await res.json() };
    } catch (e) {
      this._error = e.message;
    }
  }

  _back() {
    this.dispatchEvent(new CustomEvent('project-back', { bubbles: true, composed: true }));
  }

  async _openChat() {
    try {
      const res = await fetch(`/api/projects/${this._projectId}/session`, { method: 'POST' });
      if (!res.ok) throw new Error(await res.text());
      const { source } = await res.json();
      window.dispatchEvent(new CustomEvent('project-chat-open', {
        detail: { source, label: this._project?.name ?? `Project ${this._projectId}` },
      }));
    } catch (e) {
      this._error = e.message;
    }
  }

  // ── Rendering ─────────────────────────────────────────────────────────────────

  _renderMember(m) {
    const isOwner = m.user_id === this._project.owner_user_id;
    const manage  = this._canManage();
    return html`
      <div class="d-flex align-items-center gap-2 py-1">
        <span style="min-width:10rem">${this._userLabel(m.user_id)}
          ${isOwner ? html`<span class="badge text-bg-light ms-1">${t('projects.share.owner')}</span>` : nothing}
        </span>
        ${isOwner ? html`
          <span class="text-muted" style="font-size:0.8rem">${t('projects.share.access.readwrite')}</span>
        ` : manage ? html`
          <div class="btn-group btn-group-sm" role="group">
            <button class="btn ${!m.can_write ? 'btn-secondary' : 'btn-outline-secondary'}"
              @click=${() => m.can_write && this._setAccess(m.user_id, false)}>
              <i class="bi bi-eye me-1"></i>${t('projects.share.access.read')}
            </button>
            <button class="btn ${m.can_write ? 'btn-secondary' : 'btn-outline-secondary'}"
              @click=${() => !m.can_write && this._setAccess(m.user_id, true)}>
              <i class="bi bi-pencil me-1"></i>${t('projects.share.access.write')}
            </button>
          </div>
          <button class="btn btn-sm btn-outline-danger" title=${t('projects.share.remove')}
            @click=${() => this._removeMember(m.user_id)}>
            <i class="bi bi-x-lg"></i>
          </button>
        ` : html`
          <span class="text-muted" style="font-size:0.8rem">
            ${m.can_write ? t('projects.share.access.readwrite') : t('projects.share.access.readonly')}
          </span>
        `}
      </div>
    `;
  }

  _renderSharePanel() {
    const candidates = this._candidates();
    const manage     = this._canManage();
    return html`
      <div class="card mb-3">
        <div class="card-body">
          <h6 class="fw-semibold mb-3"><i class="bi bi-people me-1"></i>${t('projects.share.title')}</h6>
          ${(this._project.members ?? []).map(m => this._renderMember(m))}

          ${manage ? html`
            <hr class="my-3" />
            ${candidates.length > 0 ? html`
              <div class="d-flex gap-2 align-items-center flex-wrap">
                <select class="form-select form-select-sm" style="max-width:16rem"
                  @change=${e => this._add = { ...this._add, user_id: e.target.value }}>
                  <option value="" ?selected=${!this._add.user_id}>${t('projects.share.choose_user')}</option>
                  ${candidates.map(u => html`
                    <option value=${u.id} ?selected=${this._add.user_id === u.id}>${u.display_name || u.username}</option>`)}
                </select>
                <select class="form-select form-select-sm" style="max-width:11rem"
                  @change=${e => this._add = { ...this._add, can_write: e.target.value === 'write' }}>
                  <option value="read"  ?selected=${!this._add.can_write}>${t('projects.share.access.readonly')}</option>
                  <option value="write" ?selected=${this._add.can_write}>${t('projects.share.access.readwrite')}</option>
                </select>
                <button class="btn btn-sm btn-primary" ?disabled=${!this._add.user_id}
                  @click=${() => this._addMember()}>
                  <i class="bi bi-plus-lg me-1"></i>${t('projects.share.add')}
                </button>
              </div>
            ` : html`<div class="text-muted" style="font-size:0.85rem">${t('projects.share.all_added')}</div>`}
          ` : nothing}
        </div>
      </div>
    `;
  }

  _renderFilesPanel() {
    // The file explorer is the future primary surface (a directory listing endpoint over
    // the project folder is a follow-on). For now, the chat's agent works in the folder.
    return html`
      <div class="card mb-3">
        <div class="card-body text-center text-muted py-4">
          <i class="bi bi-folder2-open" style="font-size:1.6rem"></i>
          <p class="mb-0 mt-2" style="font-size:0.9rem">${t('projects.files.placeholder')}</p>
        </div>
      </div>
    `;
  }

  render() {
    if (!this._project) {
      return html`
        <div style="display:flex;align-items:center;justify-content:center;flex:1">
          <span class="spinner-border text-primary"></span>
        </div>
      `;
    }

    return html`
      <div class="project-page">
        <div class="project-page-header">
          <div style="display:flex;align-items:center;gap:12px">
            <button class="btn btn-sm btn-outline-secondary" @click=${() => this._back()}>
              <i class="bi bi-arrow-left me-1"></i>${t('project_board.back')}
            </button>
            <h2 class="project-page-title">
              <i class="bi bi-folder2"></i>${this._project.name}
            </h2>
            ${this._project.is_owner
              ? html`<span class="badge text-bg-light"><i class="bi bi-person me-1"></i>${t('projects.badge.owned')}</span>`
              : html`<span class="badge text-bg-light"><i class="bi bi-people me-1"></i>${t('projects.badge.shared_by', { name: this._project.owner_name })}</span>`}
          </div>
          <div style="display:flex;gap:0.5rem">
            <button class="btn btn-sm btn-outline-primary" @click=${() => this._openChat()}>
              <i class="bi bi-chat-dots me-1"></i>${t('project_board.open_chat')}
            </button>
          </div>
        </div>

        ${this._error ? html`
          <div class="alert alert-danger py-2 mx-3 mt-3 mb-0" style="font-size:0.85rem">${this._error}</div>
        ` : nothing}

        <div class="p-3">
          ${this._project.description
            ? html`<p class="text-muted" style="font-size:0.9rem">${this._project.description}</p>`
            : nothing}
          ${this._renderFilesPanel()}
          ${this._renderSharePanel()}
        </div>
      </div>
    `;
  }
}
