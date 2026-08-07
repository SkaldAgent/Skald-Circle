import { html, nothing } from 'lit';
import { LightElement } from '../../lib/base.js';
import { t }            from '../../lib/i18n.js';
import { fileWatcher }  from '../../lib/file-watcher.js';

/// The Files tab of a project board: a live explorer over the project folder.
///
/// One directory at a time (`GET /api/files/dir`); clicking a folder navigates
/// into it, clicking a file opens it in the existing viewer (`window.openFile`).
/// The breadcrumb is rooted at the project folder (shown as `/`). The listing
/// reloads in real time: the shared `/api/file/watch` socket (the `fileWatcher`
/// singleton) pushes a `changed` event for the open directory whenever another
/// member — or the agent, from inside its container — creates/modifies/removes
/// a file in it. Write actions (new folder, upload, rename, delete) are offered
/// only to members with `can_write` and are gated server-side too.
export class ProjectFilesPanel extends LightElement {
  static properties = {
    project:  { attribute: false },
    _rel:     { state: true },
    _entries: { state: true },
    _loading: { state: true },
    _error:   { state: true },
    _busy:    { state: true },
    _modal:   { state: true },
    _drag:    { state: true },
  };

  constructor() {
    super();
    this.project      = null;
    this._rel         = '';        // path relative to the project root ('' = root)
    this._entries     = null;
    this._loading     = false;
    this._error       = null;
    this._busy        = false;
    this._modal       = null;      // { mode: 'mkdir'|'rename', name, target? }
    this._drag        = false;
    this._unwatch     = null;
    this._reloadTimer = null;
    this._onChanged   = () => this._scheduleReload();
  }

  willUpdate(changed) {
    // (Re)open the root only when the project itself changes — a refetch of the
    // same project (member edits) must not reset the current folder.
    if (changed.has('project')) {
      const prev = changed.get('project');
      if (this.project?.root_path && this.project.root_path !== prev?.root_path) {
        this._open('');
      }
    }
  }

  disconnectedCallback() {
    this._unwatch?.();
    clearTimeout(this._reloadTimer);
    super.disconnectedCallback();
  }

  _dirPath() {
    const root = this.project?.root_path ?? '';
    return this._rel ? `${root}/${this._rel}` : root;
  }

  async _open(rel) {
    this._unwatch?.();
    this._unwatch = null;
    this._rel   = rel;
    this._error = null;
    await this._load();
    // Live updates for the open directory (best-effort: a dead watcher just
    // means manual refresh; auto-reconnect + re-subscribe are handled inside).
    try {
      this._unwatch = await fileWatcher.watch(this._dirPath(), this._onChanged);
    } catch { this._unwatch = null; }
  }

  _scheduleReload() {
    clearTimeout(this._reloadTimer);
    this._reloadTimer = setTimeout(() => this._load(), 300);
  }

  async _load() {
    if (!this.project?.root_path) return;
    this._loading = true;
    try {
      const res = await fetch(`/api/files/dir?path=${encodeURIComponent(this._dirPath())}`);
      if (!res.ok) throw new Error(await res.text());
      this._entries = await res.json();
      this._error   = null;
    } catch (e) {
      this._error = e.message;
    } finally {
      this._loading = false;
    }
  }

  // ── Navigation ────────────────────────────────────────────────────────────

  _enter(entry) {
    if (entry.is_dir) {
      this._open(this._rel ? `${this._rel}/${entry.name}` : entry.name);
    } else {
      window.openFile(entry.path);
    }
  }

  _goTo(index) {
    // -1 = project root, otherwise the segment index to land on.
    const segs = this._rel ? this._rel.split('/') : [];
    this._open(index < 0 ? '' : segs.slice(0, index + 1).join('/'));
  }

  // ── Write actions ─────────────────────────────────────────────────────────

  _openModal(mode, target = null) {
    this._modal = { mode, name: target?.name ?? '', target };
    this.updateComplete.then(() => this.querySelector('.pf-modal-input')?.focus());
  }

  async _submitModal(e) {
    e.preventDefault();
    const name = (this._modal?.name ?? '').trim();
    if (!name || name.includes('/') || name.includes('\\')) {
      this._error = t('projects.files.error.name');
      return;
    }
    this._busy = true;
    try {
      let res;
      if (this._modal.mode === 'mkdir') {
        res = await fetch('/api/file', {
          method:  'POST',
          headers: { 'Content-Type': 'application/json' },
          body:    JSON.stringify({ path: `${this._dirPath()}/${name}`, dir: true }),
        });
      } else {
        res = await fetch('/api/file', {
          method:  'PATCH',
          headers: { 'Content-Type': 'application/json' },
          body:    JSON.stringify({ old_path: this._modal.target.path, new_path: `${this._dirPath()}/${name}` }),
        });
      }
      if (!res.ok) throw new Error(await res.text());
      this._modal = null;
      await this._load();
    } catch (err) {
      this._error = err.message;
    } finally {
      this._busy = false;
    }
  }

  async _remove(entry) {
    const key = entry.is_dir ? 'projects.files.confirm.delete_dir' : 'projects.files.confirm.delete_file';
    if (!confirm(t(key, { name: entry.name }))) return;
    this._busy = true;
    try {
      const res = await fetch(`/api/file?path=${encodeURIComponent(entry.path)}`, { method: 'DELETE' });
      if (!res.ok) throw new Error(await res.text());
      await this._load();
    } catch (e) {
      this._error = e.message;
    } finally {
      this._busy = false;
    }
  }

  async _uploadFiles(files) {
    if (!files?.length) return;
    this._busy  = true;
    this._error = null;
    try {
      for (const f of files) {
        const target = `${this._dirPath()}/${f.name}`;
        const res = await fetch(`/api/file/upload?path=${encodeURIComponent(target)}`, {
          method: 'POST',
          body:   f,
        });
        if (!res.ok) throw new Error(`${f.name}: ${await res.text()}`);
      }
      // The watcher will also fire; reload now in case it is down.
      await this._load();
    } catch (e) {
      this._error = e.message;
    } finally {
      this._busy = false;
    }
  }

  _pickFiles() {
    this.querySelector('.pf-file-input')?.click();
  }

  // ── Rendering ─────────────────────────────────────────────────────────────

  _renderBreadcrumb() {
    const segs = this._rel ? this._rel.split('/') : [];
    return html`
      <nav class="d-flex align-items-center flex-wrap" aria-label="breadcrumb"
           style="--bs-breadcrumb-divider: '/';">
        <ol class="breadcrumb mb-0" style="font-size:0.9rem">
          <li class="breadcrumb-item ${segs.length === 0 ? 'active' : ''}">
            ${segs.length === 0
              ? html`<span title=${this.project.root_path}><i class="bi bi-hdd me-1"></i>/</span>`
              : html`<a href="#" @click=${e => { e.preventDefault(); this._goTo(-1); }}
                        title=${this.project.root_path}><i class="bi bi-hdd me-1"></i>/</a>`}
          </li>
          ${segs.map((s, i) => html`
            <li class="breadcrumb-item ${i === segs.length - 1 ? 'active' : ''}">
              ${i === segs.length - 1
                ? html`<span>${s}</span>`
                : html`<a href="#" @click=${e => { e.preventDefault(); this._goTo(i); }}>${s}</a>`}
            </li>
          `)}
        </ol>
      </nav>
    `;
  }

  _renderToolbar() {
    const canWrite = !!this.project?.can_write;
    return html`
      <div class="d-flex align-items-center gap-2 mb-2">
        ${this._renderBreadcrumb()}
        <div class="ms-auto d-flex gap-1">
          <button class="btn btn-sm btn-outline-secondary" title=${t('projects.files.refresh')}
            ?disabled=${this._loading} @click=${() => this._load()}>
            <i class="bi bi-arrow-clockwise"></i>
          </button>
          <a class="btn btn-sm btn-outline-secondary" download
            href=${`/api/file/download?path=${encodeURIComponent(this._dirPath())}`}>
            <i class="bi bi-file-zip me-1"></i>${t('projects.files.btn.download')}
          </a>
          ${canWrite ? html`
            <button class="btn btn-sm btn-outline-secondary" ?disabled=${this._busy}
              @click=${() => this._openModal('mkdir')}>
              <i class="bi bi-folder-plus me-1"></i>${t('projects.files.btn.new_folder')}
            </button>
            <button class="btn btn-sm btn-outline-primary" ?disabled=${this._busy}
              @click=${() => this._pickFiles()}>
              ${this._busy
                ? html`<span class="spinner-border spinner-border-sm me-1"></span>${t('projects.files.uploading')}`
                : html`<i class="bi bi-upload me-1"></i>${t('projects.files.btn.upload')}`}
            </button>
            <input type="file" class="pf-file-input" multiple hidden
              @change=${e => { this._uploadFiles([...e.target.files]); e.target.value = ''; }} />
          ` : nothing}
        </div>
      </div>
    `;
  }

  _iconFor(entry) {
    if (entry.is_dir) return 'bi-folder-fill text-warning';
    const ext = entry.name.includes('.') ? entry.name.split('.').pop().toLowerCase() : '';
    const map = {
      png: 'bi-file-image', jpg: 'bi-file-image', jpeg: 'bi-file-image',
      gif: 'bi-file-image', webp: 'bi-file-image', svg: 'bi-file-image',
      pdf: 'bi-file-pdf',
      md: 'bi-file-text', txt: 'bi-file-text', tex: 'bi-file-text', latex: 'bi-file-text',
      js: 'bi-file-code', ts: 'bi-file-code', py: 'bi-file-code', rs: 'bi-file-code',
      json: 'bi-file-code', html: 'bi-file-code', css: 'bi-file-code', sh: 'bi-file-code',
      zip: 'bi-file-zip', gz: 'bi-file-zip', tar: 'bi-file-zip',
      mp3: 'bi-file-music', wav: 'bi-file-music', ogg: 'bi-file-music',
      mp4: 'bi-file-play', mov: 'bi-file-play', webm: 'bi-file-play',
      doc: 'bi-file-word', docx: 'bi-file-word',
      xls: 'bi-file-excel', xlsx: 'bi-file-excel', csv: 'bi-file-excel',
    };
    return map[ext] ?? 'bi-file-earmark';
  }

  _fmtDate(iso) {
    if (!iso) return '—';
    const d = new Date(iso);
    return isNaN(d) ? '—' : d.toLocaleString();
  }

  _fmtSize(n) {
    if (n == null) return '—';
    if (n < 1024) return `${n} B`;
    if (n < 1024 ** 2) return `${(n / 1024).toFixed(1)} KB`;
    if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} MB`;
    return `${(n / 1024 ** 3).toFixed(2)} GB`;
  }

  _renderRow(entry) {
    const canWrite = !!this.project?.can_write;
    return html`
      <tr style="cursor:pointer" @click=${() => this._enter(entry)}>
        <td style="width:2rem"><i class="bi ${this._iconFor(entry)}"></i></td>
        <td style="word-break:break-all">${entry.name}</td>
        <td class="text-muted text-nowrap" style="font-size:0.82rem">${this._fmtDate(entry.created_at)}</td>
        <td class="text-muted text-nowrap" style="font-size:0.82rem">${this._fmtDate(entry.modified_at)}</td>
        <td class="text-muted text-end text-nowrap" style="font-size:0.82rem">${entry.is_dir ? '—' : this._fmtSize(entry.size)}</td>
        <td class="text-end text-nowrap" @click=${e => e.stopPropagation()}>
          <a class="btn btn-sm btn-link text-secondary p-0 me-2" download
            title=${t('projects.files.action.download')}
            href=${entry.is_dir
              ? `/api/file/download?path=${encodeURIComponent(entry.path)}`
              : `/api/file?path=${encodeURIComponent(entry.path)}&force_download=true`}>
            <i class="bi bi-download"></i>
          </a>
          ${canWrite ? html`
            <button class="btn btn-sm btn-link text-secondary p-0 me-2" title=${t('projects.files.action.rename')}
              @click=${() => this._openModal('rename', entry)}>
              <i class="bi bi-pencil"></i>
            </button>
            <button class="btn btn-sm btn-link text-danger p-0" title=${t('projects.files.action.delete')}
              ?disabled=${this._busy} @click=${() => this._remove(entry)}>
              <i class="bi bi-trash"></i>
            </button>
          ` : nothing}
        </td>
      </tr>
    `;
  }

  _renderTable() {
    if (!this._entries) {
      return html`<div class="text-center py-4"><span class="spinner-border spinner-border-sm text-primary"></span></div>`;
    }
    if (this._entries.length === 0) {
      return html`
        <div class="text-center text-muted py-4">
          <i class="bi bi-folder2-open" style="font-size:1.4rem"></i>
          <p class="mb-0 mt-2" style="font-size:0.88rem">${t('projects.files.empty')}</p>
        </div>
      `;
    }
    return html`
      <table class="table table-sm table-hover align-middle mb-0">
        <thead>
          <tr>
            <th></th>
            <th>${t('projects.files.col.name')}</th>
            <th style="width:9.5rem">${t('projects.files.col.created')}</th>
            <th style="width:9.5rem">${t('projects.files.col.modified')}</th>
            <th class="text-end" style="width:5.5rem">${t('projects.files.col.size')}</th>
            <th style="width:6rem"></th>
          </tr>
        </thead>
        <tbody>
          ${this._entries.map(e => this._renderRow(e))}
        </tbody>
      </table>
    `;
  }

  _renderModal() {
    if (!this._modal) return nothing;
    const isMkdir = this._modal.mode === 'mkdir';
    return html`
      <div class="agent-dialog-backdrop"
           @click=${e => { if (e.target === e.currentTarget) this._modal = null; }}>
        <div class="agent-dialog">
          <div style="display:flex;align-items:center;gap:8px;margin-bottom:1rem">
            <i class="bi ${isMkdir ? 'bi-folder-plus' : 'bi-pencil'}"></i>
            <span style="font-weight:600">
              ${isMkdir ? t('projects.files.modal.mkdir') : t('projects.files.modal.rename', { name: this._modal.target.name })}
            </span>
            <button type="button" style="margin-left:auto;border:none;background:none;cursor:pointer;font-size:1.1rem"
              @click=${() => this._modal = null}>
              <i class="bi bi-x"></i>
            </button>
          </div>
          <form @submit=${e => this._submitModal(e)}>
            <div class="mb-4">
              <label class="form-label fw-semibold" style="font-size:0.82rem">${t('projects.files.modal.name')}</label>
              <input type="text" class="form-control form-control-sm pf-modal-input" required
                .value=${this._modal.name}
                @input=${e => this._modal = { ...this._modal, name: e.target.value }} />
            </div>
            <div style="display:flex;justify-content:flex-end;gap:0.5rem">
              <button type="button" class="btn btn-sm btn-outline-secondary"
                @click=${() => this._modal = null}>${t('projects.modal.cancel')}</button>
              <button type="submit" class="btn btn-sm btn-primary" ?disabled=${this._busy}>
                <i class="bi bi-check-lg me-1"></i>${isMkdir ? t('projects.modal.create') : t('projects.modal.save')}
              </button>
            </div>
          </form>
        </div>
      </div>
    `;
  }

  render() {
    if (!this.project?.root_path) return nothing;
    const canWrite = !!this.project?.can_write;
    return html`
      <div class="card ${this._drag ? 'border-primary' : ''}"
        @dragover=${e => { if (canWrite) { e.preventDefault(); this._drag = true; } }}
        @dragleave=${() => this._drag = false}
        @drop=${e => { e.preventDefault(); this._drag = false; if (canWrite) this._uploadFiles([...e.dataTransfer.files]); }}>
        <div class="card-body">
          ${this._renderToolbar()}
          ${this._error ? html`
            <div class="alert alert-danger py-2 mb-2" style="font-size:0.85rem">${this._error}</div>
          ` : nothing}
          ${this._drag ? html`
            <div class="text-center text-primary py-3" style="font-size:0.9rem">
              <i class="bi bi-cloud-arrow-up me-1"></i>${t('projects.files.drop')}
            </div>
          ` : this._renderTable()}
        </div>
      </div>
      ${this._renderModal()}
    `;
  }
}
