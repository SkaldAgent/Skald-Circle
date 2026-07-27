import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';
import { t, I18nMixin } from '../lib/i18n.js';


// ── Navigation model ──────────────────────────────────────────────────────────
// The nav is data, not markup: every entry declares its `group` and a numeric
// `priority` (lower = higher up), and each group is rendered by sorting its
// entries on that key. Retuning the order is editing a number here, never moving
// JSX around. Plugin-contributed pages (`GET /api/plugins/pages`) carry their own
// `priority` (see `PluginPage`) and merge into the `workspace` group on the *same*
// number line — the convention is core items live in 10–90 and plugin pages ≥100,
// so a page lands after the daily items by default yet stays freely placeable.
//
// The split axis is **function**, not permission: `adminOnly`/`debugOnly` gate
// individual entries, and a whole section disappears when it has no visible entry
// for this user (so "Configuration" — all admin entries — vanishes for non-admins
// without a section-level role check). `aliases` lists extra `_activePage` values
// that should light the entry (e.g. the detail route paired with its list).
const NAV = [
  // Il tuo spazio — daily productivity. Visible to everyone (full mode).
  { id: 'home',           group: 'workspace',  priority: 10, icon: 'chat-dots',       labelKey: 'nav.chat' },
  { id: 'inbox',          group: 'workspace',  priority: 20, icon: 'inbox',           labelKey: 'nav.inbox' },
  { id: 'dashboard',      group: 'workspace',  priority: 30, icon: 'speedometer2',    labelKey: 'nav.dashboard' },
  { id: 'projects',       group: 'workspace',  priority: 40, icon: 'kanban',          labelKey: 'nav.projects' },
  { id: 'tasks',          group: 'workspace',  priority: 50, icon: 'lightning-charge',labelKey: 'nav.tasks' },
  // Shared folders is admin-managed but *content*, so it lives with the daily
  // items, not buried in Configuration — the link stays admin-gated per-entry.
  { id: 'shared-folders', group: 'workspace',  priority: 60, icon: 'folder-symlink',  labelKey: 'nav.shared_folders', adminOnly: true },

  // Estensioni — what the assistant is made of / can use. Visible to everyone;
  // Agents is read-only for non-admins (editable only by the admin server-side).
  { id: 'connectors',     group: 'extensions', priority: 10, icon: 'plug',            labelKey: 'nav.connectors', aliases: ['connector'] },
  { id: 'plugins',        group: 'extensions', priority: 20, icon: 'puzzle',          labelKey: 'nav.plugins' },
  { id: 'agents',         group: 'extensions', priority: 30, icon: 'people',          labelKey: 'nav.agents' },
  // The background agents the instance runs for you. Visible to everyone: the
  // run log is the caller's own, so there is nothing here to gate on a role.
  { id: 'system-agents',  group: 'extensions', priority: 40, icon: 'robot',           labelKey: 'nav.system_agents' },

  // Configurazione — rarely-touched setup. Every entry is admin-only today, so
  // the section is admin-only in effect via the empty-section rule.
  { id: 'users',          group: 'config',     priority: 10, icon: 'person-badge',    labelKey: 'nav.users',          adminOnly: true },
  { id: 'roles',          group: 'config',     priority: 20, icon: 'tags',            labelKey: 'nav.roles',          adminOnly: true },
  { id: 'models',         group: 'config',     priority: 30, icon: 'cpu',             labelKey: 'nav.models',         adminOnly: true },
  { id: 'providers',      group: 'config',     priority: 40, icon: 'plug',            labelKey: 'nav.providers',      adminOnly: true },
  { id: 'approval',       group: 'config',     priority: 50, icon: 'shield-check',    labelKey: 'nav.security',       adminOnly: true },
  { id: 'plugin-catalog', group: 'config',     priority: 70, icon: 'puzzle-fill',     labelKey: 'nav.plugin_catalog', adminOnly: true, aliases: ['plugin-detail'] },
  { id: 'catalog',        group: 'config',     priority: 80, icon: 'journal-text',    labelKey: 'nav.catalog',        adminOnly: true, aliases: ['marketplace'] },
  { id: 'config',         group: 'config',     priority: 90, icon: 'gear',            labelKey: 'nav.config',         adminOnly: true },

  // Sviluppo — debug surface, only with the debug flag on.
  { id: 'llm-requests',   group: 'dev',        priority: 10, icon: 'journal-code',    labelKey: 'nav.llm_requests',   debugOnly: true },
];

// Section order + which sections collapse. Configuration and Development are
// collapsible and closed by default (rarely touched); the two productivity
// sections are always open.
const GROUPS = [
  { id: 'workspace',  labelKey: 'nav.section.workspace',  collapsible: false },
  { id: 'extensions', labelKey: 'nav.section.extensions', collapsible: false },
  { id: 'config',     labelKey: 'nav.section.config',     collapsible: true  },
  { id: 'dev',        labelKey: 'nav.section.dev',        collapsible: true  },
];

const COLLAPSE_KEY = 'sidebar-collapsed';

// The projects NAV entry, surfaced in the simplified interface too (projects are
// membership-gated, not capability-gated, so a simple-mode member can own/share
// them just like anyone else).
const PROJECTS_NAV = NAV.find((i) => i.id === 'projects');


export class AppSidebar extends I18nMixin(LightElement) {
  static properties = {
    _activePage:    { state: true },
    _tasksSection:  { state: true },
    _inboxCount:    { state: true },
    _debugMode:     { state: true },
    _recentProjects: { state: true },
    _me:            { state: true },
    _pluginPages:   { state: true },
    _collapsed:     { state: true },
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
    this._collapsed      = { config: true, dev: true };
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
    // Poll inbox count independently of whether the page is open. The 60 s
    // interval is only a fallback: `inbox-changed` (pushed over the chat WS
    // when any session raises/settles a pending item) refreshes it live.
    this._pollInbox();
    this._pollTimer = setInterval(() => this._pollInbox(), 60000);
    window.addEventListener('inbox-changed', () => this._pollInbox());
    this._loadCollapsed();
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

  // Persisted per-section collapse state. Defaults (config + dev closed) apply
  // until the user toggles a section, then their choice is remembered.
  _loadCollapsed() {
    try {
      const saved = JSON.parse(localStorage.getItem(COLLAPSE_KEY) || '{}');
      this._collapsed = { config: true, dev: true, ...saved };
    } catch { /* keep defaults */ }
  }

  _toggleSection(id) {
    this._collapsed = { ...this._collapsed, [id]: !this._collapsed[id] };
    try { localStorage.setItem(COLLAPSE_KEY, JSON.stringify(this._collapsed)); } catch { /* ignore */ }
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
    return ['inbox', 'dashboard', 'tasks', 'projects', 'models', 'providers', 'approval', 'agents', 'users', 'roles', 'shared-folders', 'connectors', 'connector', 'plugins', 'plugin-catalog', 'plugin-detail', 'catalog', 'marketplace', 'profile', 'config', 'llm-requests', 'session', 'system-agents', 'file_viewer', 'tool_detail'].includes(segment) ? segment : 'home';
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

  // ── Entry-level helpers ─────────────────────────────────────────────────────

  _itemVisible(item) {
    if (item.adminOnly && this._me?.role_id !== 'admin') return false;
    if (item.debugOnly && !this._debugMode) return false;
    return true;
  }

  _isActive(item) {
    if (this._activePage === item.id) return true;
    return (item.aliases || []).includes(this._activePage);
  }

  // Core entries of a group plus (workspace only) the plugin pages, merged on the
  // shared `priority` line and sorted ascending (lower = higher up).
  _entriesForGroup(groupId) {
    const core = NAV
      .filter((i) => i.group === groupId && this._itemVisible(i))
      .map((i) => ({ kind: 'core', priority: i.priority, item: i }));
    const plugins = groupId === 'workspace'
      ? this._pluginPages.map((p) => ({ kind: 'plugin', priority: p.priority ?? 100, page: p }))
      : [];
    return [...core, ...plugins].sort((a, b) => a.priority - b.priority);
  }

  // ── Render helpers ──────────────────────────────────────────────────────────

  _renderGroup(group) {
    const entries = this._entriesForGroup(group.id);
    if (!entries.length) return nothing;   // empty section → hidden
    const collapsed = group.collapsible && this._collapsed[group.id];
    const header = group.collapsible
      ? html`
        <button class="sidebar-section-label is-collapsible"
                @click=${() => this._toggleSection(group.id)}>
          <span>${t(group.labelKey)}</span>
          <i class="bi bi-chevron-${collapsed ? 'down' : 'up'}"></i>
        </button>`
      : html`<div class="sidebar-section-label"><span>${t(group.labelKey)}</span></div>`;
    return html`
      <div class="sidebar-section">
        ${header}
        ${collapsed ? nothing : entries.map((e) => this._renderEntry(e))}
      </div>`;
  }

  _renderEntry(entry) {
    if (entry.kind === 'plugin') return this._renderPluginEntry(entry.page);
    const item = entry.item;
    switch (item.id) {
      case 'home':     return this._renderHome();
      case 'inbox':    return this._renderInbox(item);
      case 'tasks':    return this._renderTasksMenu();
      case 'projects': return html`${this._renderStdLink(item)}${this._renderRecentProjects()}`;
      default:         return this._renderStdLink(item);
    }
  }

  _renderStdLink(item) {
    return html`
      <a href="#${item.id}" class="sidebar-link ${this._isActive(item) ? 'active' : ''}"
         @click=${(e) => this._togglePage(item.id, e)}>
        <i class="bi bi-${item.icon}"></i>
        <span class="sidebar-link-name">${t(item.labelKey)}</span>
      </a>`;
  }

  _renderHome() {
    return html`
      <a href="#" class="sidebar-link ${this._activePage === 'home' ? 'active' : ''}"
         @click=${(e) => this._togglePage('home', e)}>
        <i class="bi bi-chat-dots"></i>
        <span class="sidebar-link-name">${t('nav.chat')}</span>
      </a>`;
  }

  _renderInbox(item) {
    return html`
      <a href="#inbox" class="sidebar-link ${this._isActive(item) ? 'active' : ''}"
         @click=${(e) => this._togglePage('inbox', e)}>
        <i class="bi bi-inbox"></i>
        <span class="sidebar-link-name">
          ${t('nav.inbox')}
          ${this._inboxCount > 0
            ? html`<span class="badge bg-danger ms-1" style="font-size:0.65rem">${this._inboxCount}</span>`
            : ''}
        </span>
      </a>`;
  }

  _renderPluginEntry(p) {
    const route = `plugin/${p.plugin_id}/${p.page_id}`;
    return html`
      <a href="#${route}"
         class="sidebar-link ${this._activePage === route ? 'active' : ''}"
         @click=${(e) => this._togglePage(route, e)}>
        <i class="bi bi-${p.icon}"></i>
        <span class="sidebar-link-name">${p.title}</span>
      </a>`;
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
    // Simplified interface (role attrs `ui_mode: "simple"`): chat, inbox and
    // projects (self-service workspaces — membership-gated, not capability-gated),
    // ungrouped. Hiding links is not access control — every route stays
    // capability-gated server-side; this only shapes the nav for less technical
    // members.
    const simple = this._me?.ui_mode === 'simple';
    return html`
      <div class="sidebar-brand">
        <img src="/assets/icons/icon-1024.png" alt="" class="sidebar-brand-icon" />
        <span>${t('topbar.brand')}</span>
      </div>

      <hr class="sidebar-divider" />

      <nav class="sidebar-nav">
        ${simple
          ? html`
              ${this._renderHome()}
              ${this._renderInbox({ id: 'inbox' })}
              ${PROJECTS_NAV ? this._renderEntry({ kind: 'core', item: PROJECTS_NAV }) : nothing}
            `
          : GROUPS.map((g) => this._renderGroup(g))}
      </nav>
    `;
  }
}
