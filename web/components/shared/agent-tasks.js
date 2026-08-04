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

/**
 * What a background task is waiting on: the approval or question it raised.
 *
 * One at a time, on purpose. A task that is blocked stays blocked whether or not
 * its card is on screen, so stacking every pending item here would trade a
 * readable chat for a queue nobody asked to see all of — the count says how many
 * are behind it, and resolving this one reveals the next.
 *
 * It sits above the strip rather than in the transcript because the transcript
 * is a record of what was said: the task that is asking may have been started
 * twenty messages ago, and a card that scrolls away is a card that gets missed.
 *
 * Which is also why it can be closed: a card that cannot be moved out of the way
 * is a card that takes the chat hostage. The ✕ hides it and nothing more — the
 * item stays pending and the Inbox stays the place to answer it.
 */
function renderPending(host) {
  const pending = host._taskPending ?? [];
  if (pending.length === 0) return nothing;

  const [item] = pending;

  return html`
    <div class="agent-task-ask">
      <div class="agent-task-ask-head">
        <button class="agent-task-ask-close"
                title=${t('chat.tasks.ask_hide')}
                @click=${() => host._dismissAsk(item)}>
          <i class="bi bi-x"></i>
        </button>
        <i class="bi bi-hand-index-thumb-fill"></i>
        <span>${t('chat.tasks.needs_you')}</span>
        <span class="agent-task-ask-job" title=${item.job_title}>${item.job_title}</span>
        ${pending.length > 1
          ? html`<span class="agent-task-ask-count">
                   ${t('chat.tasks.pending_n', { n: pending.length })}
                 </span>`
          : nothing}
      </div>
      ${host._inboxError
        ? html`<div class="agent-task-ask-error">${host._inboxError}</div>`
        : nothing}
      ${item.kind === 'approval'
        ? host._renderApprovalCard(item)
        : host._renderClarificationCard(item)}
    </div>
  `;
}

export function renderTaskStrip(host) {
  const tasks   = host._tasks ?? [];
  const pending = host._taskPending ?? [];
  const hidden  = host._taskPendingHidden ?? 0;
  // A pending item outlives the strip's own drop timers, so it keeps the
  // container alive on its own: a task can still be waiting on a human after
  // its row has been dismissed.
  //
  // Dismissed items deliberately do *not* keep it alive. Closing the last card
  // with nothing else running has to leave a clean chat, or the ✕ would not be
  // the promise it looks like — the sidebar badge and the Inbox are where those
  // items live now.
  if (tasks.length === 0 && pending.length === 0) return nothing;

  const running = tasks.filter(x => x.state === 'running').length;

  return html`
    <div class="agent-tasks">
      ${renderPending(host)}
      ${tasks.length > 0 ? html`
        <div class="agent-tasks-head">
          <i class="bi bi-cpu"></i>
          <span>${running > 0
            ? t('chat.tasks.running_n', { n: running })
            : t('chat.tasks.title')}</span>
          ${hidden > 0
            ? html`<a class="agent-tasks-hidden" href="#inbox">
                     <i class="bi bi-inbox"></i> ${t('chat.tasks.hidden_n', { n: hidden })}
                   </a>`
            : nothing}
          <a class="agent-tasks-all" href="#tasks">${t('chat.tasks.see_all')}</a>
        </div>
        ${tasks.map(task => renderTask(host, task))}
      ` : nothing}
    </div>
  `;
}
