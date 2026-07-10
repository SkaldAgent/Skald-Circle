import { html, nothing } from 'lit';
import { LightElement } from '../../lib/base.js';
import { formatDate, formatDuration } from './utils.js';

export class TaskHistorySection extends LightElement {
  static properties = {
    _runs:     { state: true },
    _error:    { state: true },
    _loading:  { state: true },
    _expanded: { state: true },
  };

  constructor() {
    super();
    this._runs     = [];
    this._error    = null;
    this._loading  = false;
    this._expanded = null;
  }

  async load() {
    this._error   = null;
    this._loading = true;
    try {
      const res = await fetch('/api/cron/runs');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      this._runs = await res.json();
    } catch (e) {
      this._error = e.message;
    } finally {
      this._loading = false;
    }
  }

  _statusClass(status) {
    return { completed: 'success', failed: 'danger', cancelled: 'warning' }[status] ?? 'secondary';
  }

  _toggleExpand(id) {
    this._expanded = this._expanded === id ? null : id;
  }

  _renderRow(run) {
    const isExpanded = this._expanded === run.id;
    return html`
      <tr class="task-history-row ${isExpanded ? 'task-history-row--expanded' : ''}"
          @click=${() => this._toggleExpand(run.id)}>
        <td>
          <span class="badge bg-${this._statusClass(run.status)}">${run.status}</span>
        </td>
        <td class="task-history-title">${run.job_title ?? `Job #${run.job_id}`}</td>
        <td>${run.agent_id ?? '—'}</td>
        <td>${formatDate(run.completed_at)}</td>
        <td>${formatDuration(run.duration_ms)}</td>
        <td><i class="bi bi-chevron-${isExpanded ? 'up' : 'down'} task-history-chevron"></i></td>
      </tr>
      ${isExpanded ? html`
        <tr class="task-history-detail">
          <td colspan="6">
            ${run.session_id != null ? html`
              <div style="margin-bottom:0.5rem">
                <a href="#session/${run.session_id}"
                   style="font-size:0.8rem;font-family:var(--bs-font-monospace)">
                  <i class="bi bi-chat-text me-1"></i>View session #${run.session_id}
                </a>
              </div>
            ` : nothing}
            ${run.error ? html`
              <div class="task-history-error"><strong>Error:</strong> ${run.error}</div>
            ` : nothing}
            ${run.final_response ? html`
              <div class="task-history-response"><pre>${run.final_response}</pre></div>
            ` : nothing}
            ${!run.error && !run.final_response ? html`
              <div class="text-muted" style="font-size:0.82rem">No output recorded.</div>
            ` : nothing}
          </td>
        </tr>
      ` : nothing}
    `;
  }

  render() {
    return html`
      <div class="task-page">
        <div class="task-page-header">
          <h2 class="task-page-title"><i class="bi bi-journal-text"></i> History</h2>
          <div style="font-size:0.82rem;color:var(--bs-secondary-color)">
            ${this._runs.length} run${this._runs.length !== 1 ? 's' : ''}
          </div>
        </div>

        ${this._error ? html`
          <div class="alert alert-danger py-2 mx-3 mb-2" style="font-size:0.85rem">${this._error}</div>
        ` : nothing}

        ${this._loading ? html`
          <div class="task-empty"><i class="bi bi-hourglass-split"></i><p>Loading…</p></div>
        ` : this._runs.length === 0 ? html`
          <div class="task-empty">
            <i class="bi bi-journal-text"></i>
            <p>No completed runs yet.</p>
          </div>
        ` : html`
          <div class="task-history-table-wrap">
            <table class="task-history-table">
              <thead>
                <tr>
                  <th>Status</th>
                  <th>Task</th>
                  <th>Agent</th>
                  <th>Completed</th>
                  <th>Duration</th>
                  <th></th>
                </tr>
              </thead>
              <tbody>
                ${this._runs.map(r => this._renderRow(r))}
              </tbody>
            </table>
          </div>
        `}
      </div>
    `;
  }
}
