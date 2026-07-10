import { html, nothing } from 'lit';
import { LightElement } from '../../lib/base.js';
import { RunningTasksSection }   from './running.js';
import { CronJobsSection }       from './cron.js';
import { ScheduledTasksSection } from './scheduled.js';
import { TaskHistorySection }    from './history.js';

const SECTIONS = ['running', 'cron', 'scheduled', 'history'];

export class TasksPage extends LightElement {
  static properties = {
    _open:    { state: true },
    _section: { state: true },
  };

  constructor() {
    super();
    this._open    = false;
    this._section = 'running';
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('llm-page-change', (e) => {
      const open = e.detail.page === 'tasks';
      this._open = open;
      this.style.display = open ? 'flex' : 'none';
      if (open) {
        const sec = this._sectionFromHash();
        this._section = sec;
        this._loadSection(sec);
        if (!location.hash.includes('/')) {
          history.replaceState({ page: 'tasks', section: sec }, '', '#tasks/' + sec);
        }
      }
    });
    window.addEventListener('tasks-section-change', (e) => {
      if (!this._open) return;
      const sec = e.detail.section;
      this._section = sec;
      this._loadSection(sec);
    });
  }

  _sectionFromHash() {
    const parts = location.hash.slice(1).split('/');
    if (parts[0] === 'tasks' && parts[1]) {
      return SECTIONS.includes(parts[1]) ? parts[1] : 'running';
    }
    return 'running';
  }

  _loadSection(sec) {
    this.updateComplete.then(() => {
      const el = this.querySelector(`[data-section="${sec}"]`);
      if (el?.load) el.load();
    });
  }

  render() {
    if (!this._open) return nothing;
    return html`
      ${this._section === 'running'
        ? html`<task-running-section data-section="running"></task-running-section>`
        : nothing}
      ${this._section === 'cron'
        ? html`<task-cron-jobs-section data-section="cron"></task-cron-jobs-section>`
        : nothing}
      ${this._section === 'scheduled'
        ? html`<task-scheduled-section data-section="scheduled"></task-scheduled-section>`
        : nothing}
      ${this._section === 'history'
        ? html`<task-history-section data-section="history"></task-history-section>`
        : nothing}
    `;
  }
}

customElements.define('task-running-section',    RunningTasksSection);
customElements.define('task-cron-jobs-section',  CronJobsSection);
customElements.define('task-scheduled-section',  ScheduledTasksSection);
customElements.define('task-history-section',    TaskHistorySection);
