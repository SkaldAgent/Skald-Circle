import { html, nothing }          from 'lit';
import { unsafeHTML }             from 'lit/directives/unsafe-html.js';
import { LightElement }           from '../../lib/base.js';
import { toString as cronToString } from 'cronstrue';
import { formatDate } from './utils.js';
import { t }          from '../../lib/i18n.js';

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

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
  }

  disconnectedCallback() {
    window.removeEventListener('locale-changed', this.__onLocaleChanged);
    super.disconnectedCallback();
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
    if (!confirm(t('cron.confirm.delete', { title: job.title }))) return;
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
      return html`<span class="task-badge task-badge--running">${t('cron.badge.running')}</span>`;
    if (!job.enabled)
      return html`<span class="task-badge task-badge--disabled">${t('cron.badge.disabled')}</span>`;
    return html`<span class="task-badge task-badge--idle">${t('cron.badge.idle')}</span>`;
  }

  _renderCard(job) {
    return html`
      <div class="task-card ${job.enabled ? '' : 'task-card--disabled'}">
        <div class="task-card-header">
          <div class="task-card-title-row">
            <span class="task-card-title">${job.title}</span>
            ${this._statusBadge(job)}
          </div>
          <button class="task-card-delete" title=${t('cron.action.delete')} @click=${() => this._delete(job)}>
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
            <span class="task-card-meta-label">${t('cron.card.label_agent')}</span>
            <span class="task-card-meta-value">${job.agent_id}</span>
          </div>
          <div class="task-card-meta-item">
            <span class="task-card-meta-label">${t('cron.card.label_last_run')}</span>
            <span class="task-card-meta-value">${formatDate(job.last_run_at)}</span>
          </div>
          <div class="task-card-meta-item">
            <span class="task-card-meta-label">${t('cron.card.label_next_run')}</span>
            <span class="task-card-meta-value">${formatDate(job.next_run_at)}</span>
          </div>
        </div>

        <div class="task-card-footer">
          <div class="form-check form-switch mb-0 task-card-toggle">
            <input class="form-check-input" type="checkbox" role="switch"
              .checked=${job.enabled}
              @change=${() => this._toggle(job)} />
            <span class="task-card-toggle-label">${job.enabled ? t('cron.card.enabled') : t('cron.card.disabled')}</span>
          </div>
        </div>
      </div>
    `;
  }

  render() {
    return html`
      <div class="task-page">
        <div class="task-page-header">
          <h2 class="task-page-title"><i class="bi bi-repeat"></i> ${t('cron.title')}</h2>
          <div style="font-size:0.82rem;color:var(--bs-secondary-color)">
            ${t(this._jobs.length === 1 ? 'cron.count_one' : 'cron.count_other', { n: this._jobs.length })}
          </div>
        </div>

        ${this._error ? html`
          <div class="alert alert-danger py-2 mx-3 mb-0" style="font-size:0.85rem">${this._error}</div>
        ` : nothing}

        ${this._jobs.length === 0 ? html`
          <div class="task-empty">
            <i class="bi bi-repeat"></i>
            <p>${t('cron.empty.title')} ${unsafeHTML(t('cron.empty.hint'))}</p>
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
