import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';
import { t, I18nMixin } from '../lib/i18n.js';


export class AppSidebar extends I18nMixin(LightElement) {
  static properties = {
    _activePage:    { state: true },
    _tasksSection:  { state: true },
    _inboxCount:    { state: true },
    _debugMode:     { state: true },
    _recentProjects: { state: true },
    _me:            { state: true },
    _pluginPages:   { state: true },
  };

  constructor() {
    super();
    this._activePage     = null;
    this._tasksSection   = 'running';
    this._inboxCount     = 0;
    this._pollTimer      = null;
    this._debugMode      = false;
    this._recentProjects = [];
    this._me             = null;
    this._pluginPages    = [];
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('popstate', (e) => {
      const page = e.state?.page ?? this._pageFromHash();
      if (page === 'tasks') this._tasksSection = this._tasksSectionFromHash();
      this._applyPage(page);
    });
    window.addEventListener('hashchange', () => {
      const page = this._pageFromHash();
      if (page === 'tasks') this._tasksSection = this._tasksSectionFromHash();
      this._applyPage(page);
    });
    window.addEventListener('inbox-count', (e) => {
      this._inboxCount = e.detail.count;
    });
    window.addEventListener('debug-mode-change', (e) => {
      this._debugMode = e.detail.enabled;
    });
    // On load: home (root) if no hash, otherwise the matching page
    setTimeout(() => {
      const page = this._pageFromHash();
      if (page === 'tasks') this._tasksSection = this._tasksSectionFromHash();
      this._applyPage(page);
    }, 0);
    // Poll inbox count independently of whether the page is open.
    this._pollInbox();
    this._pollTimer = setInterval(() => this._pollInbox(), 10000);
    this._loadDebugMode();
    this._loadRecentProjects();
    this._loadMe();
    this._loadPluginPages();
    window.addEventListener('plugins-changed', () => this._loadPluginPages());
    window.addEventListener('project-updated', () => this._loadRecentProjects());
  }

  // Only for deciding which links to draw. Hiding a link is not access control —
  // every admin route is capability-gated server-side (`require_cap`), so this
  // only avoids offering a door that would answer 403.
  async _loadMe() {
    try {
      const res = await fetch('/api/auth/me');
      if (res.ok) this._me = await res.json();
    } catch { /* ignore */ }
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    clearInterval(this._pollTimer);
  }

  async _loadDebugMode() {
    try {
      const res = await fetch('/api/dev/debug_mode');
      if (res.ok) this._debugMode = (await res.json()).enabled;
    } catch { /* ignore */ }
  }

  async _loadRecentProjects() {
    try {
      const res = await fetch('/api/projects');
      if (!res.ok) return;
      const projects = await res.json();
      this._recentProjects = projects
        .slice()
        .sort((a, b) => new Date(b.updated_at) - new Date(a.updated_at))
        .slice(0, 5);
    } catch { /* ignore */ }
  }

  async _openProjectChat(projectId, projectName, e) {
    e.preventDefault();
    e.stopPropagation();
    try {
      const res = await fetch(`/api/projects/${projectId}/session`, { method: 'POST' });
      if (!res.ok) return;
      const { source } = await res.json();
      window.dispatchEvent(new CustomEvent('project-chat-open', {
        detail: { source, label: projectName },
      }));
    } catch { /* ignore */ }
  }

  async _pollInbox() {
    try {
      const res = await fetch('/api/inbox');
      if (res.ok) {
        const data = await res.json();
        this._inboxCount = data.total ?? 0;
      }
    } catch { /* ignore */ }
  }

  // Plugin-contributed menu entries (`GET /api/plugins/pages`, per-user).
  // Refetched on `plugins-changed` (fired by the plugins admin pages after an
  // enable/disable) so entries appear/disappear without a reload.
  async _loadPluginPages() {
    try {
      const res = await fetch('/api/plugins/pages');
      if (res.ok) this._pluginPages = await res.json();
    } catch { /* ignore */ }
  }

  _pageFromHash() {
    const hash = location.hash.slice(1);
    if (!hash) return 'home';
    // Segment ends at the first `/` (e.g. `#session/123`) or `?` (e.g. `#file_viewer?path=...`).
    const match = hash.match(/^([^/?]+)/);
    const segment = match ? match[1] : '';
    // Plugin pages: `#plugin/<plugin_id>/<page_id>` — the route is accepted by
    // shape (deep links must survive the async `/api/plugins/pages` load); the
    // host reports an error if the page turns out not to exist for this user.
    if (segment === 'plugin') {
      const m = hash.match(/^plugin\/([^/?]+)\/([^/?]+)/);
      return m ? `plugin/${m[1]}/${m[2]}` : 'home';
    }
    // `connector` (singular) is the per-connector detail page, `connectors` the list.
    return ['inbox', 'dashboard', 'tasks', 'projects', 'models', 'providers', 'approval', 'agents', 'users', 'roles', 'shared-folders', 'connectors', 'connector', 'plugins', 'plugin-catalog', 'plugin-detail', 'catalog', 'marketplace', 'profile', 'config', 'llm-requests', 'session', 'tic', 'file_viewer'].includes(segment) ? segment : 'home';
  }

  _tasksSectionFromHash() {
    const parts = location.hash.slice(1).split('/');
    if (parts[0] === 'tasks' && parts[1]) {
      return ['running', 'cron', 'scheduled', 'history'].includes(parts[1]) ? parts[1] : 'running';
    }
    return 'running';
  }

  _applyPage(page) {
    this._activePage = page;
    window.dispatchEvent(new CustomEvent('llm-page-change', { detail: { page } }));
  }

  _togglePage(page, e) {
    e.preventDefault();
    if (page === 'home') {
      history.pushState({ page: 'home' }, '', location.pathname + location.search);
      this._applyPage('home');
      return;
    }
    if (this._activePage === page) {
      // In a sub-section (e.g. #models/image) → go back to page root
      if (location.hash.slice(1) !== page) {
        history.pushState({ page }, '', '#' + page);
        this._applyPage(page);
      }
      return;
    }
    history.pushState({ page }, '', '#' + page);
    this._applyPage(page);
  }

  _navigateTasksSection(sec, e) {
    e.preventDefault();
    this._tasksSection = sec;
    history.pushState({ page: 'tasks', section: sec }, '', '#tasks/' + sec);
    if (this._activePage !== 'tasks') {
      this._applyPage('tasks');
    } else {
      // page already open — tell the TasksPage to switch section
      window.dispatchEvent(new CustomEvent('tasks-section-change', { detail: { section: sec } }));
    }
  }

  _openTaskManager(e) {
    e.preventDefault();
    if (this._activePage === 'tasks') return; // already open, submenu visible
    const sec = this._tasksSection || 'cron';
    history.pushState({ page: 'tasks', section: sec }, '', '#tasks/' + sec);
    this._applyPage('tasks');
  }

  _renderTasksMenu() {
    const active = this._activePage === 'tasks';
    const sec    = this._tasksSection;
    return html`
      <a href="#tasks/cron"
         class="sidebar-link ${active ? 'active' : ''}"
         @click=${(e) => this._openTaskManager(e)}>
        <i class="bi bi-lightning-charge"></i>
        <span class="sidebar-link-name">${t('nav.tasks')}</span>
        <i class="bi bi-chevron-${active ? 'up' : 'down'} sidebar-link-chevron"></i>
      </a>
      ${active ? html`
        <div class="sidebar-submenu">
          <a href="#tasks/running"
             class="sidebar-sublink ${sec === 'running' ? 'active' : ''}"
             @click=${(e) => this._navigateTasksSection('running', e)}>
            <i class="bi bi-activity"></i> ${t('nav.tasks.running')}
          </a>
          <a href="#tasks/cron"
             class="sidebar-sublink ${sec === 'cron' ? 'active' : ''}"
             @click=${(e) => this._navigateTasksSection('cron', e)}>
            <i class="bi bi-repeat"></i> ${t('nav.tasks.cron')}
          </a>
          <a href="#tasks/scheduled"
             class="sidebar-sublink ${sec === 'scheduled' ? 'active' : ''}"
             @click=${(e) => this._navigateTasksSection('scheduled', e)}>
            <i class="bi bi-clock"></i> ${t('nav.tasks.scheduled')}
          </a>
          <a href="#tasks/history"
             class="sidebar-sublink ${sec === 'history' ? 'active' : ''}"
             @click=${(e) => this._navigateTasksSection('history', e)}>
            <i class="bi bi-journal-text"></i> ${t('nav.tasks.history')}
          </a>
        </div>
      ` : nothing}
    `;
  }

  _renderPluginPages() {
    if (!this._pluginPages.length) return nothing;
    return html`
      <hr class="sidebar-divider" />
      ${this._pluginPages.map(p => {
        const route = `plugin/${p.plugin_id}/${p.page_id}`;
        return html`
          <a href="#${route}"
             class="sidebar-link ${this._activePage === route ? 'active' : ''}"
             @click=${(e) => this._togglePage(route, e)}>
            <i class="bi bi-${p.icon}"></i>
            <span class="sidebar-link-name">${p.title}</span>
          </a>`;
      })}
    `;
  }

  _renderRecentProjects() {
    if (!this._recentProjects.length) return nothing;
    return html`
      <div class="sidebar-submenu">
        ${this._recentProjects.map(p => html`
          <div class="sidebar-project-link"
               @click=${(e) => { e.preventDefault(); history.pushState({ page: 'projects' }, '', '#projects'); this._applyPage('projects'); window.dispatchEvent(new CustomEvent('sidebar-open-project', { detail: { id: p.id } })); }}>
            <i class="bi bi-folder2" style="font-size:0.78rem;opacity:0.65;flex-shrink:0"></i>
            <span class="sidebar-project-name">${p.name}</span>
            <button class="sidebar-project-chat-btn"
                    title=${t('topbar.open_chat')}
                    @click=${(e) => this._openProjectChat(p.id, p.name, e)}>
              <i class="bi bi-chat-dots"></i>
            </button>
          </div>
        `)}
      </div>
    `;
  }

  render() {
    // Simplified interface (role attrs `ui_mode: "simple"`): chat + inbox only.
    // Hiding links is not access control — every route stays capability-gated
    // server-side; this only shapes the navigation for less technical members.
    const simple = this._me?.ui_mode === 'simple';
    return html`
      <div class="sidebar-brand">
        <img src="/assets/icons/icon-1024.png" alt="" class="sidebar-brand-icon" />
        <span>${t('topbar.brand')}</span>
      </div>

      <hr class="sidebar-divider" />

      <nav class="sidebar-nav">
        <a href="#" class="sidebar-link ${this._activePage === 'home' ? 'active' : ''}"
           @click=${(e) => this._togglePage('home', e)}>
          <i class="bi bi-chat-dots"></i>
          <span class="sidebar-link-name">${t('nav.chat')}</span>
        </a>

        <a href="#inbox" class="sidebar-link ${this._activePage === 'inbox' ? 'active' : ''}"
           @click=${(e) => this._togglePage('inbox', e)}>
          <i class="bi bi-inbox"></i>
          <span class="sidebar-link-name">
            ${t('nav.inbox')}
            ${this._inboxCount > 0
              ? html`<span class="badge bg-danger ms-1" style="font-size:0.65rem">${this._inboxCount}</span>`
              : ''}
          </span>
        </a>

        ${simple ? nothing : html`
        <a href="#dashboard"
           class="sidebar-link ${this._activePage === 'dashboard' ? 'active' : ''}"
           @click=${(e) => this._togglePage('dashboard', e)}>
          <i class="bi bi-speedometer2"></i>
          <span class="sidebar-link-name">${t('nav.dashboard')}</span>
        </a>

        <a href="#projects"
           class="sidebar-link ${this._activePage === 'projects' ? 'active' : ''}"
           @click=${(e) => this._togglePage('projects', e)}>
          <i class="bi bi-kanban"></i>
          <span class="sidebar-link-name">${t('nav.projects')}</span>
        </a>
        ${this._renderRecentProjects()}

        ${this._renderTasksMenu()}

        <a href="#" class="sidebar-link ${this._activePage === 'models' ? 'active' : ''}"
           @click=${(e) => this._togglePage('models', e)}>
          <i class="bi bi-cpu"></i>
          <span class="sidebar-link-name">${t('nav.models')}</span>
        </a>
        <a href="#" class="sidebar-link ${this._activePage === 'providers' ? 'active' : ''}"
           @click=${(e) => this._togglePage('providers', e)}>
          <i class="bi bi-plug"></i>
          <span class="sidebar-link-name">${t('nav.providers')}</span>
        </a>
        <a href="#" class="sidebar-link ${this._activePage === 'approval' ? 'active' : ''}"
           @click=${(e) => this._togglePage('approval', e)}>
          <i class="bi bi-shield-check"></i>
          <span class="sidebar-link-name">${t('nav.security')}</span>
        </a>
        <a href="#" class="sidebar-link ${this._activePage === 'agents' ? 'active' : ''}"
           @click=${(e) => this._togglePage('agents', e)}>
          <i class="bi bi-people"></i>
          <span class="sidebar-link-name">${t('nav.agents')}</span>
        </a>
        <a href="#" class="sidebar-link ${this._activePage === 'users' ? 'active' : ''}"
           @click=${(e) => this._togglePage('users', e)}>
          <i class="bi bi-person-badge"></i>
          <span class="sidebar-link-name">${t('nav.users')}</span>
        </a>
        <a href="#" class="sidebar-link ${this._activePage === 'roles' ? 'active' : ''}"
           @click=${(e) => this._togglePage('roles', e)}>
          <i class="bi bi-tags"></i>
          <span class="sidebar-link-name">${t('nav.roles')}</span>
        </a>
        ${this._me?.role_id === 'admin' ? html`
          <a href="#" class="sidebar-link ${this._activePage === 'shared-folders' ? 'active' : ''}"
             @click=${(e) => this._togglePage('shared-folders', e)}>
            <i class="bi bi-folder-symlink"></i>
            <span class="sidebar-link-name">${t('nav.shared_folders')}</span>
          </a>` : nothing}
        <a href="#" class="sidebar-link ${this._activePage === 'connectors' || this._activePage === 'connector' ? 'active' : ''}"
           @click=${(e) => this._togglePage('connectors', e)}>
          <i class="bi bi-plug"></i>
          <span class="sidebar-link-name">${t('nav.connectors')}</span>
        </a>
        <a href="#" class="sidebar-link ${this._activePage === 'plugins' ? 'active' : ''}"
           @click=${(e) => this._togglePage('plugins', e)}>
          <i class="bi bi-puzzle"></i>
          <span class="sidebar-link-name">${t('nav.plugins')}</span>
        </a>
        ${this._me?.role_id === 'admin' ? html`
          <a href="#" class="sidebar-link ${this._activePage === 'plugin-catalog' || this._activePage === 'plugin-detail' ? 'active' : ''}"
             @click=${(e) => this._togglePage('plugin-catalog', e)}>
            <i class="bi bi-puzzle-fill"></i>
            <span class="sidebar-link-name">${t('nav.plugin_catalog')}</span>
          </a>` : nothing}
        ${this._me?.role_id === 'admin' ? html`
          <a href="#" class="sidebar-link ${this._activePage === 'catalog' || this._activePage === 'marketplace' ? 'active' : ''}"
             @click=${(e) => this._togglePage('catalog', e)}>
            <i class="bi bi-journal-text"></i>
            <span class="sidebar-link-name">${t('nav.catalog')}</span>
          </a>` : nothing}
        <a href="#" class="sidebar-link ${this._activePage === 'config' ? 'active' : ''}"
           @click=${(e) => this._togglePage('config', e)}>
          <i class="bi bi-gear"></i>
          <span class="sidebar-link-name">${t('nav.config')}</span>
        </a>

        ${this._renderPluginPages()}

        ${this._debugMode ? html`
          <hr class="sidebar-divider" />
          <a href="#llm-requests"
             class="sidebar-link ${this._activePage === 'llm-requests' ? 'active' : ''}"
             @click=${(e) => this._togglePage('llm-requests', e)}>
            <i class="bi bi-journal-code"></i>
            <span class="sidebar-link-name">${t('nav.llm_requests')}</span>
          </a>
          <a href="#tic"
             class="sidebar-link ${this._activePage === 'tic' ? 'active' : ''}"
             @click=${(e) => this._togglePage('tic', e)}>
            <i class="bi bi-bell"></i>
            <span class="sidebar-link-name">${t('nav.tic')}</span>
          </a>
        ` : nothing}
        `}
      </nav>

    `;
  }
}
