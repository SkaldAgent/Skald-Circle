import { html, nothing } from 'lit';
import { LightElement } from '../../lib/base.js';
import { toString as cronToString } from 'cronstrue';
import { formatDate } from './utils.js';

export class CronJobsSection extends LightElement {
  static properties = {
    _jobs:  { state: true },
    _error: { state: true },
  };

  constructor() {
    super();
    this._jobs  = [];
    this._error = null;
  }

  async load() {
    this._error = null;
    try {
      const res = await fetch('/api/cron/jobs');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const allJobs = await res.json();
      this._jobs = allJobs.filter(j => j.kind === 'cron' && !j.single_run);
    } catch (e) {
      this._error = e.message;
    }
  }

  async _delete(job) {
    if (!confirm(`Delete job "${job.title}"?`)) return;
    try {
      const res = await fetch(`/api/cron/jobs/${job.id}`, { method: 'DELETE' });
      if (!res.ok) throw new Error(await res.text());
      await this.load();
    } catch (e) { this._error = e.message; }
  }

  async _toggle(job) {
    try {
      const res = await fetch(`/api/cron/jobs/${job.id}/toggle`, {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ enabled: !job.enabled }),
      });
      if (!res.ok) throw new Error(await res.text());
      await this.load();
    } catch (e) { this._error = e.message; }
  }

  _statusBadge(job) {
    if (job.running_session_id != null)
      return html`<span class="task-badge task-badge--running">running</span>`;
    if (!job.enabled)
      return html`<span class="task-badge task-badge--disabled">disabled</span>`;
    return html`<span class="task-badge task-badge--idle">idle</span>`;
  }

  _renderCard(job) {
    return html`
      <div class="task-card ${job.enabled ? '' : 'task-card--disabled'}">
        <div class="task-card-header">
          <div class="task-card-title-row">
            <span class="task-card-title">${job.title}</span>
            ${this._statusBadge(job)}
          </div>
          <button class="task-card-delete" title="Delete" @click=${() => this._delete(job)}>
            <i class="bi bi-trash"></i>
          </button>
        </div>

        ${job.description ? html`<div class="task-card-desc">${job.description}</div>` : nothing}

        <div class="task-card-expr">
          <i class="bi bi-clock"></i>
          <div class="task-card-expr-text">
            <span class="task-card-human">${cronToString(job.cron)}</span>
            <code class="task-card-raw">${job.cron}</code>
          </div>
        </div>

        <div class="task-card-meta">
          <div class="task-card-meta-item">
            <span class="task-card-meta-label">Agent</span>
            <span class="task-card-meta-value">${job.agent_id}</span>
          </div>
          <div class="task-card-meta-item">
            <span class="task-card-meta-label">Last run</span>
            <span class="task-card-meta-value">${formatDate(job.last_run_at)}</span>
          </div>
          <div class="task-card-meta-item">
            <span class="task-card-meta-label">Next run</span>
            <span class="task-card-meta-value">${formatDate(job.next_run_at)}</span>
          </div>
        </div>

        <div class="task-card-footer">
          <div class="form-check form-switch mb-0 task-card-toggle">
            <input class="form-check-input" type="checkbox" role="switch"
              .checked=${job.enabled}
              @change=${() => this._toggle(job)} />
            <span class="task-card-toggle-label">${job.enabled ? 'Enabled' : 'Disabled'}</span>
          </div>
        </div>
      </div>
    `;
  }

  render() {
    return html`
      <div class="task-page">
        <div class="task-page-header">
          <h2 class="task-page-title"><i class="bi bi-repeat"></i> Cron Jobs</h2>
          <div style="font-size:0.82rem;color:var(--bs-secondary-color)">
            ${this._jobs.length} job${this._jobs.length !== 1 ? 's' : ''}
          </div>
        </div>

        ${this._error ? html`
          <div class="alert alert-danger py-2 mx-3 mb-0" style="font-size:0.85rem">${this._error}</div>
        ` : nothing}

        ${this._jobs.length === 0 ? html`
          <div class="task-empty">
            <i class="bi bi-repeat"></i>
            <p>No recurring cron jobs. Ask the agent to create one with <code>execute_task</code>.</p>
          </div>
        ` : html`
          <div class="task-grid">
            ${this._jobs.map(j => this._renderCard(j))}
          </div>
        `}
      </div>
    `;
  }
}
