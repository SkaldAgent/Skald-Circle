import { html, nothing } from 'lit';
import { LightElement }  from '../../lib/base.js';

/**
 * Mobile project list. Lists projects from `GET /api/projects`; tapping one opens
 * (or resumes) its coordinator session via `POST /api/projects/{id}/session` and
 * emits a `project-open` event so the shell can re-point the chat to that source.
 *
 * The session machinery is shared with the desktop copilot: the same `project-{id}`
 * source, the same provisioning (`project-coordinator` + project RunContext), so a
 * project chat is continuous across desktop, mobile browser, and the native shell.
 */
export class ProjectsPage extends LightElement {
  static properties = {
    visible:  { type: Boolean },
    _data:    { state: true },
    _error:   { state: true },
    _loading: { state: true },
  };

  constructor() {
    super();
    this.visible  = false;
    this._data    = null;
    this._error   = null;
    this._loading = false;
  }

  updated(changed) {
    if (changed.has('visible') && this.visible) this._load();
  }

  async _load() {
    this._loading = true;
    try {
      const res = await fetch('/api/projects');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      this._data  = await res.json();
      this._error = null;
    } catch (e) {
      this._error = e.message;
    } finally {
      this._loading = false;
    }
  }

  async _open(project) {
    try {
      const res = await fetch(`/api/projects/${project.id}/session`, { method: 'POST' });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const { source } = await res.json();
      this.dispatchEvent(new CustomEvent('project-open', {
        bubbles: true, composed: true,
        detail: { source, label: project.name },
      }));
    } catch (e) {
      this._error = e.message;
    }
  }

  render() {
    if (!this.visible) return nothing;
    const projects = this._data ?? [];

    return html`
      <div class="mobile-projects">
        <div class="mobile-section-header">
          <span class="mobile-section-title">
            <i class="bi bi-folder2-open"></i> Projects
          </span>
          <button class="inbox-refresh-btn" @click=${() => this._load()}>
            <i class="bi bi-arrow-clockwise"></i>
          </button>
        </div>

        ${this._error ? html`
          <div class="mobile-alert-error">${this._error}</div>
        ` : nothing}

        ${projects.length === 0 ? html`
          <div class="inbox-empty">
            <i class="bi bi-folder"></i>
            <p>${this._loading ? 'Loading…' : 'No projects yet'}</p>
          </div>
        ` : html`
          <div class="mobile-projects-list">
            ${projects.map(p => html`
              <div class="project-card" @click=${() => this._open(p)}>
                <div class="project-card-icon">
                  <i class="bi bi-folder2-open"></i>
                </div>
                <div class="project-card-main">
                  <div class="project-card-name">${p.name}</div>
                  ${p.description ? html`
                    <div class="project-card-desc">${p.description}</div>
                  ` : nothing}
                </div>
                <i class="bi bi-chevron-right project-card-chevron"></i>
              </div>
            `)}
          </div>
        `}
      </div>
    `;
  }
}

customElements.define('projects-page', ProjectsPage);
