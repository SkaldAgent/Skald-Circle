import { html, nothing } from 'lit';
import { LightElement } from '../../lib/base.js';
import { formatDate } from '../tasks/utils.js';

export class ProjectListSection extends LightElement {
  static properties = {
    _projects: { state: true },
    _modal:    { state: true },
    _form:     { state: true },
    _saving:   { state: true },
    _error:    { state: true },
  };

  constructor() {
    super();
    this._projects = [];
    this._modal    = null;
    this._form     = this._emptyForm();
    this._saving   = false;
    this._error    = null;
  }

  _emptyForm() {
    return { name: '', path: '', description: '' };
  }

  async load() {
    this._error = null;
    try {
      const res = await fetch('/api/projects');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      this._projects = await res.json();
    } catch (e) {
      this._error = e.message;
    }
  }

  _openAdd() {
    this._form  = this._emptyForm();
    this._error = null;
    this._modal = { mode: 'add' };
  }

  _openEdit(project) {
    this._form  = { name: project.name, path: project.path, description: project.description ?? '' };
    this._error = null;
    this._modal = { mode: 'edit', project };
  }

  _closeModal() {
    this._modal = null;
    this._error = null;
  }

  async _submit(e) {
    e.preventDefault();
    if (this._saving) return;
    this._saving = true;
    this._error  = null;
    const isEdit = this._modal?.mode === 'edit';
    const url    = isEdit ? `/api/projects/${this._modal.project.id}` : '/api/projects';
    try {
      const res = await fetch(url, {
        method:  isEdit ? 'PUT' : 'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify(this._form),
      });
      if (!res.ok) throw new Error(await res.text());
      this._modal = null;
      await this.load();
    } catch (err) {
      this._error = err.message;
    } finally {
      this._saving = false;
    }
  }

  async _delete(project) {
    if (!confirm(`Delete project "${project.name}"?\nAll tickets will also be deleted.`)) return;
    try {
      const res = await fetch(`/api/projects/${project.id}`, { method: 'DELETE' });
      if (!res.ok) throw new Error(await res.text());
      await this.load();
    } catch (e) {
      this._error = e.message;
    }
  }

  _navigate(project) {
    this.dispatchEvent(new CustomEvent('project-navigate', {
      bubbles: true, composed: true, detail: { id: project.id },
    }));
  }

  _setField(f, v) {
    this._form = { ...this._form, [f]: v };
  }

  _renderModal() {
    const isEdit = this._modal?.mode === 'edit';
    return html`
      <div class="agent-dialog-backdrop"
           @click=${e => { if (e.target === e.currentTarget) this._closeModal(); }}>
        <div class="agent-dialog">
          <div style="display:flex;align-items:center;gap:8px;margin-bottom:1rem">
            <i class="bi bi-kanban"></i>
            <span style="font-weight:600">${isEdit ? 'Edit Project' : 'New Project'}</span>
            <button type="button" style="margin-left:auto;border:none;background:none;cursor:pointer;font-size:1.1rem"
              @click=${() => this._closeModal()}>
              <i class="bi bi-x"></i>
            </button>
          </div>

          ${this._error ? html`
            <div class="alert alert-danger py-2 mb-3" style="font-size:0.85rem">${this._error}</div>
          ` : nothing}

          <form @submit=${e => this._submit(e)}>
            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">Name</label>
              <input type="text" class="form-control form-control-sm" required
                placeholder="My Project"
                .value=${this._form.name}
                @input=${e => this._setField('name', e.target.value)} />
            </div>
            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">Path</label>
              <input type="text" class="form-control form-control-sm" required
                placeholder="/path/to/project"
                .value=${this._form.path}
                @input=${e => this._setField('path', e.target.value)} />
            </div>
            <div class="mb-4">
              <label class="form-label fw-semibold" style="font-size:0.82rem">Description</label>
              <textarea class="form-control form-control-sm" rows="2"
                placeholder="What this project is about"
                .value=${this._form.description}
                @input=${e => this._setField('description', e.target.value)}></textarea>
            </div>
            <div style="display:flex;justify-content:flex-end;gap:0.5rem">
              <button type="button" class="btn btn-sm btn-outline-secondary"
                @click=${() => this._closeModal()}>Cancel</button>
              <button type="submit" class="btn btn-sm btn-primary" ?disabled=${this._saving}>
                ${this._saving
                  ? html`<span class="spinner-border spinner-border-sm me-1"></span>Saving…`
                  : html`<i class="bi bi-check-lg me-1"></i>${isEdit ? 'Save' : 'Create'}`}
              </button>
            </div>
          </form>
        </div>
      </div>
    `;
  }

  _renderCard(project) {
    return html`
      <div class="project-card" @click=${() => this._navigate(project)}>
        <div class="project-card-header">
          <div class="project-card-title">${project.name}</div>
          <div class="project-card-actions" @click=${e => e.stopPropagation()}>
            <button class="project-card-icon-btn" title="Edit"
              @click=${() => this._openEdit(project)}>
              <i class="bi bi-pencil"></i>
            </button>
            <button class="project-card-icon-btn project-card-icon-btn--danger" title="Delete"
              @click=${() => this._delete(project)}>
              <i class="bi bi-trash"></i>
            </button>
          </div>
        </div>
        <div class="project-card-path"><i class="bi bi-folder2 me-1"></i>${project.path}</div>
        ${project.description
          ? html`<div class="project-card-desc">${project.description}</div>`
          : nothing}
        <div class="project-card-meta">Updated ${formatDate(project.updated_at)}</div>
      </div>
    `;
  }

  render() {
    return html`
      <div class="project-page">
        <div class="project-page-header">
          <h2 class="project-page-title"><i class="bi bi-kanban"></i> Projects</h2>
          <button class="btn btn-sm btn-primary" @click=${() => this._openAdd()}>
            <i class="bi bi-plus-lg me-1"></i>New Project
          </button>
        </div>

        ${this._error ? html`
          <div class="alert alert-danger py-2 mx-3 mt-3 mb-0" style="font-size:0.85rem">${this._error}</div>
        ` : nothing}

        ${this._projects.length === 0 ? html`
          <div class="task-empty">
            <i class="bi bi-kanban"></i>
            <p>No projects yet. Create one to get started.</p>
          </div>
        ` : html`
          <div class="project-grid">
            ${this._projects.map(p => this._renderCard(p))}
          </div>
        `}

        ${this._modal ? this._renderModal() : nothing}
      </div>
    `;
  }
}
