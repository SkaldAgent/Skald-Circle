import { html, nothing } from 'lit';
import { t } from '../../lib/i18n.js';

/**
 * The background-task strip: what the agent handed off to run on its own.
 *
 * Rendered just above the composer, and only when there is something to say —
 * an empty strip would be a permanent slice of chrome for an occasional event.
 * It sits outside the message flow on purpose: a task started twenty messages
 * ago scrolls away exactly when it matters most, and the transcript is a record
 * of what was said, not a dashboard.
 *
 * State lives on the chat component (`ChatSession._tasks`), which owns the
 * WebSocket the updates arrive on; this file only knows how it looks. Shared by
 * the desktop copilot and the mobile chat page.
 */

const STATE_ICON = {
  running:   'bi-arrow-repeat agent-task-spin',
  completed: 'bi-check-circle-fill',
  failed:    'bi-exclamation-triangle-fill',
  cancelled: 'bi-slash-circle',
};

/** Elapsed time since an ISO timestamp, coarse on purpose (`12s`, `4m 03s`). */
function elapsed(startedAt) {
  if (!startedAt) return '';
  const ms = Date.now() - new Date(startedAt).getTime();
  if (!Number.isFinite(ms) || ms < 0) return '';
  const s = Math.floor(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${String(s % 60).padStart(2, '0')}s`;
  return `${Math.floor(m / 60)}h ${String(m % 60).padStart(2, '0')}m`;
}

function renderTask(host, task) {
  const running = task.state === 'running';
  // The task's own session page is the drill-in: it already shows, live, what a
  // background agent is doing. Without it "a task is running" is a fact you can
  // do nothing with.
  const openable = task.session_id != null && host._canOpenTaskSession;

  return html`
    <div class="agent-task agent-task--${task.state}">
      <i class="bi ${STATE_ICON[task.state] ?? 'bi-hourglass'} agent-task-icon"></i>

      <button
        class="agent-task-body"
        ?disabled=${!openable}
        title=${openable ? t('chat.tasks.open') : ''}
        @click=${() => { if (openable) window.location.hash = `session/${task.session_id}`; }}
      >
        <span class="agent-task-title">${task.title}</span>
        <span class="agent-task-meta">
          ${task.agent_id}
          ${running ? html`· ${elapsed(task.started_at)}` : nothing}
          ${task.state === 'failed'    ? html`· ${t('chat.tasks.failed')}`    : nothing}
          ${task.state === 'completed' ? html`· ${t('chat.tasks.completed')}` : nothing}
          ${task.state === 'cancelled' ? html`· ${t('chat.tasks.cancelled')}` : nothing}
        </span>
        ${task.error && task.state === 'failed'
          ? html`<span class="agent-task-error" title=${task.error}>${task.error}</span>`
          : nothing}
      </button>

      ${running
        ? html`<button class="agent-task-action" title=${t('chat.tasks.stop')}
                       @click=${() => host._stopTask(task.job_id)}>
                 <i class="bi bi-stop-fill"></i>
               </button>`
        : html`<button class="agent-task-action" title=${t('chat.tasks.dismiss')}
                       @click=${() => host._dismissTask(task.job_id)}>
                 <i class="bi bi-x"></i>
               </button>`}
    </div>
  `;
}

export function renderTaskStrip(host) {
  const tasks = host._tasks ?? [];
  if (tasks.length === 0) return nothing;

  const running = tasks.filter(x => x.state === 'running').length;

  return html`
    <div class="agent-tasks">
      <div class="agent-tasks-head">
        <i class="bi bi-cpu"></i>
        <span>${running > 0
          ? t('chat.tasks.running_n', { n: running })
          : t('chat.tasks.title')}</span>
        <a class="agent-tasks-all" href="#tasks">${t('chat.tasks.see_all')}</a>
      </div>
      ${tasks.map(task => renderTask(host, task))}
    </div>
  `;
}
