import { html, nothing } from 'lit';
import { LightElement } from '../../lib/base.js';
import { ProjectListSection }  from './project-list.js';
import { ProjectBoardSection } from './project-board.js';

export class ProjectsPage extends LightElement {
  static properties = {
    _open:      { state: true },
    _view:      { state: true },
    _projectId: { state: true },
  };

  constructor() {
    super();
    this._open      = false;
    this._view      = 'list';
    this._projectId = null;
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('llm-page-change', (e) => {
      const open = e.detail.page === 'projects';
      this._open = open;
      this.style.display = open ? 'flex' : 'none';
      if (open) {
        const { view, id } = this._parseHash();
        this._view      = view;
        this._projectId = id;
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
      return { view: 'board', id: parseInt(parts[1], 10) };
    }
    return { view: 'list', id: null };
  }

  _loadCurrent() {
    this.updateComplete.then(() => {
      if (this._view === 'list') {
        this.querySelector('project-list-section')?.load();
      } else {
        this.querySelector('project-board-section')?.load(this._projectId);
      }
    });
  }

  _navigateToBoard(id) {
    this._view      = 'board';
    this._projectId = id;
    history.pushState({ page: 'projects', id }, '', `#projects/${id}`);
    this.updateComplete.then(() => {
      this.querySelector('project-board-section')?.load(id);
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
        ></project-board-section>
      `}
    `;
  }
}

customElements.define('project-list-section',  ProjectListSection);
customElements.define('project-board-section', ProjectBoardSection);
