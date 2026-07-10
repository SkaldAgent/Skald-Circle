import { html, nothing } from 'lit';
import { LightElement } from '../../lib/base.js';
import { toString as cronToString } from 'cronstrue';
import { formatDate } from './utils.js';

export class ScheduledTasksSection extends LightElement {
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
      const all = await res.json();
      // one-shot scheduled or async/immediate, still active (pending or running)
      this._jobs = all.filter(j =>
        (j.kind !== 'cron' || j.single_run) &&
        (j.enabled || j.running_session_id != null)
      );
    } catch (e) {
      this._error = e.message;
    }
  }

  async _delete(job) {
    if (!confirm(`Delete task "${job.title}"?`)) return;
    try {
      const res = await fetch(`/api/cron/jobs/${job.id}`, { method: 'DELETE' });
      if (!res.ok) throw new Error(await res.text());
      await this.load();
    } catch (e) { this._error = e.message; }
  }

  _statusBadge(job) {
    if (job.running_session_id != null)
      return html`<span class="task-badge task-badge--running">running</span>`;
    if (job.kind === 'cron' && job.single_run)
      return html`<span class="task-badge task-badge--pending">pending</span>`;
    return html`<span class="task-badge task-badge--pending">queued</span>`;
  }

  _kindLabel(job) {
    if (job.kind === 'cron' && job.single_run)
      return html`<span class="task-badge task-badge--oneshot">one-shot</span>`;
    if (job.kind === 'async')
      return html`<span class="task-badge task-badge--async">async</span>`;
    return nothing;
  }

  _renderCard(job) {
    return html`
      <div class="task-card">
        <div class="task-card-header">
          <div class="task-card-title-row">
            <span class="task-card-title">${job.title}</span>
            ${this._kindLabel(job)}
            ${this._statusBadge(job)}
          </div>
          <button class="task-card-delete" title="Delete" @click=${() => this._delete(job)}>
            <i class="bi bi-trash"></i>
          </button>
        </div>

        ${job.description ? html`<div class="task-card-desc">${job.description}</div>` : nothing}

        ${job.kind === 'cron' && job.cron ? html`
          <div class="task-card-expr">
            <i class="bi bi-clock"></i>
            <div class="task-card-expr-text">
              <span class="task-card-human">${cronToString(job.cron)}</span>
              <code class="task-card-raw">${job.cron}</code>
            </div>
          </div>
        ` : nothing}

        <div class="task-card-meta">
          <div class="task-card-meta-item">
            <span class="task-card-meta-label">Agent</span>
            <span class="task-card-meta-value">${job.agent_id}</span>
          </div>
          <div class="task-card-meta-item">
            <span class="task-card-meta-label">${job.next_run_at ? 'Scheduled at' : 'Created'}</span>
            <span class="task-card-meta-value">${formatDate(job.next_run_at || job.created_at)}</span>
          </div>
        </div>
      </div>
    `;
  }

  render() {
    return html`
      <div class="task-page">
        <div class="task-page-header">
          <h2 class="task-page-title"><i class="bi bi-clock"></i> Scheduled Tasks</h2>
          <div style="font-size:0.82rem;color:var(--bs-secondary-color)">
            ${this._jobs.length} task${this._jobs.length !== 1 ? 's' : ''}
          </div>
        </div>

        ${this._error ? html`
          <div class="alert alert-danger py-2 mx-3 mb-0" style="font-size:0.85rem">${this._error}</div>
        ` : nothing}

        ${this._jobs.length === 0 ? html`
          <div class="task-empty">
            <i class="bi bi-clock"></i>
            <p>No pending or running tasks.</p>
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
