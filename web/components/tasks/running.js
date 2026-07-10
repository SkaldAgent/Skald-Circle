import { html, nothing } from 'lit';
import { LightElement } from '../../lib/base.js';
import { formatDate, formatElapsed } from './utils.js';

export class RunningTasksSection extends LightElement {
  static properties = {
    _jobs:    { state: true },
    _error:   { state: true },
    _tick:    { state: true },
    _killing: { state: true },
  };

  constructor() {
    super();
    this._jobs      = [];
    this._error     = null;
    this._tick      = 0;
    this._killing   = new Set();
    this._timer     = null;
    this._pollTimer = null;
  }

  connectedCallback() {
    super.connectedCallback();
    this._timer     = setInterval(() => { this._tick++; }, 1000);
    this._pollTimer = setInterval(() => this.load(), 10000);
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    clearInterval(this._timer);
    clearInterval(this._pollTimer);
  }

  async load() {
    this._error = null;
    try {
      const res = await fetch('/api/cron/jobs');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const all = await res.json();
      this._jobs = all.filter(j => j.running_session_id != null);
    } catch (e) {
      this._error = e.message;
    }
  }

  async _kill(job) {
    const confirmed = window.confirm(
      `Terminate task "${job.title}"?\n\nThe task will be stopped immediately and recorded as failed.`
    );
    if (!confirmed) return;

    this._killing = new Set([...this._killing, job.id]);
    try {
      const res = await fetch(`/api/cron/jobs/${job.id}/kill`, { method: 'POST' });
      if (!res.ok) {
        const text = await res.text();
        throw new Error(text || `HTTP ${res.status}`);
      }
      await this.load();
    } catch (e) {
      this._error = `Failed to terminate task: ${e.message}`;
    } finally {
      const next = new Set(this._killing);
      next.delete(job.id);
      this._killing = next;
    }
  }

  _kindLabel(job) {
    if (job.kind === 'cron' && !job.single_run)
      return html`<span class="task-badge task-badge--cron">cron</span>`;
    if (job.kind === 'cron' && job.single_run)
      return html`<span class="task-badge task-badge--oneshot">one-shot</span>`;
    return html`<span class="task-badge task-badge--async">async</span>`;
  }

  _renderCard(job) {
    const killing = this._killing.has(job.id);
    return html`
      <div class="task-card task-card--running">
        <div class="task-card-header">
          <div class="task-card-title-row">
            <span class="task-card-title">${job.title}</span>
            ${this._kindLabel(job)}
            <span class="task-badge task-badge--running">running</span>
          </div>
          <button
            class="task-kill-btn"
            title="Terminate task"
            ?disabled=${killing}
            @click=${() => this._kill(job)}
          >
            ${killing
              ? html`<i class="bi bi-hourglass-split"></i>`
              : html`<i class="bi bi-x-lg"></i>`}
          </button>
        </div>

        ${job.description ? html`<div class="task-card-desc">${job.description}</div>` : nothing}

        <div class="task-running-elapsed">
          <i class="bi bi-stopwatch"></i>
          <span>${formatElapsed(job.running_since)}</span>
          <span class="task-running-since">since ${formatDate(job.running_since)}</span>
        </div>

        <div class="task-card-meta">
          <div class="task-card-meta-item">
            <span class="task-card-meta-label">Agent</span>
            <span class="task-card-meta-value">${job.agent_id}</span>
          </div>
          <div class="task-card-meta-item">
            <span class="task-card-meta-label">Session</span>
            <a href="#session/${job.running_session_id}"
               class="task-card-meta-value task-card-session-link">#${job.running_session_id}</a>
          </div>
        </div>
      </div>
    `;
  }

  render() {
    void this._tick; // drives re-render every second for live elapsed time
    return html`
      <div class="task-page">
        <div class="task-page-header">
          <h2 class="task-page-title"><i class="bi bi-activity"></i> Running Tasks</h2>
          <div style="font-size:0.82rem;color:var(--bs-secondary-color)">
            ${this._jobs.length} running
          </div>
        </div>

        ${this._error ? html`
          <div class="alert alert-danger py-2 mx-3 mb-0" style="font-size:0.85rem">${this._error}</div>
        ` : nothing}

        ${this._jobs.length === 0 ? html`
          <div class="task-empty">
            <i class="bi bi-activity"></i>
            <p>No tasks currently running.</p>
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
