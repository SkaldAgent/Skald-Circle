import { html, nothing } from 'lit';
import { LightElement } from '../../lib/base.js';
import { ProjectListSection }  from './project-list.js';
import { ProjectBoardSection } from './project-board.js';

export class ProjectsPage extends LightElement {
  static properties = {
    _open:      { state: true },
    _view:      { state: true },
    _projectId: { state: true },
    _tab:       { state: true },
  };

  constructor() {
    super();
    this._open      = false;
    this._view      = 'list';
    this._projectId = null;
    this._tab       = 'files';
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('llm-page-change', (e) => {
      const open = e.detail.page === 'projects';
      this._open = open;
      this.style.display = open ? 'flex' : 'none';
      if (open) {
        const { view, id, tab } = this._parseHash();
        this._view      = view;
        this._projectId = id;
        this._tab       = tab;
        this._loadCurrent();
      }
    });
    window.addEventListener('hashchange', () => {
      // Back/forward (or manual edit) between board tabs: same project → just
      // switch the tab, no reload; anything else → re-sync from the hash.
      if (!this._open || !location.hash.startsWith('#projects')) return;
      const { view, id, tab } = this._parseHash();
      if (view === 'board' && this._view === 'board' && id === this._projectId) {
        this._tab = tab;
        this.querySelector('project-board-section')?.setTab(tab);
      } else {
        this._view      = view;
        this._projectId = id;
        this._tab       = tab;
        this._loadCurrent();
      }
    });
    window.addEventListener('sidebar-open-project', (e) => {
      this._open = true;
      this.style.display = 'flex';
      this._navigateToBoard(e.detail.id);
    });
  }

  _parseHash() {
    const parts = location.hash.slice(1).split('/');
    if (parts[0] === 'projects' && parts[1] && /^\d+$/.test(parts[1])) {
      const tab = parts[2] === 'sharing' ? 'sharing' : 'files';
      return { view: 'board', id: parseInt(parts[1], 10), tab };
    }
    return { view: 'list', id: null, tab: 'files' };
  }

  _loadCurrent() {
    this.updateComplete.then(() => {
      if (this._view === 'list') {
        this.querySelector('project-list-section')?.load();
      } else {
        this.querySelector('project-board-section')?.load(this._projectId, this._tab);
      }
    });
  }

  _navigateToBoard(id) {
    this._view      = 'board';
    this._projectId = id;
    this._tab       = 'files';
    history.pushState({ page: 'projects', id }, '', `#projects/${id}`);
    this.updateComplete.then(() => {
      this.querySelector('project-board-section')?.load(id, 'files');
    });
  }

  _navigateToList() {
    this._view      = 'list';
    this._projectId = null;
    history.pushState({ page: 'projects' }, '', '#projects');
    this.updateComplete.then(() => {
      this.querySelector('project-list-section')?.load();
    });
  }

  _onTabChange(tab) {
    this._tab = tab;
    const hash = tab === 'sharing' ? `#projects/${this._projectId}/sharing` : `#projects/${this._projectId}`;
    history.pushState({ page: 'projects', id: this._projectId }, '', hash);
  }

  render() {
    if (!this._open) return nothing;
    return html`
      ${this._view === 'list' ? html`
        <project-list-section
          @project-navigate=${e => this._navigateToBoard(e.detail.id)}
        ></project-list-section>
      ` : html`
        <project-board-section
          @project-back=${() => this._navigateToList()}
          @project-tab-change=${e => this._onTabChange(e.detail.tab)}
        ></project-board-section>
      `}
    `;
  }
}

customElements.define('project-list-section',  ProjectListSection);
customElements.define('project-board-section', ProjectBoardSection);
