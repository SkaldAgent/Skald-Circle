import { LitElement, html, nothing } from 'lit';
import './shared/inbox-page.js';
import './shared/chat-page.js';
import './shared/projects-page.js';
import './shared/file-viewer-mobile.js';

// Sections addressable via the URL hash — same routing style as the desktop
// sidebar (web/components/sidebar.js). The native iOS shell and mobile browsers
// share this router so the URL always reflects the active section: native menu
// sync, deep links, and back/refresh restoration all flow from one place.
// `file_viewer` is not a tab — it's opened from content (a clickable tool path
// via openFile() → `#file_viewer?path=...`) and so has no bottom-nav entry.
const VALID_SECTIONS = ['inbox', 'projects', 'chat', 'notifications', 'settings', 'file_viewer'];

class MobileApp extends LitElement {
  // No shadow DOM — lets external CSS and Bootstrap Icons apply directly.
  createRenderRoot() { return this; }

  static properties = {
    _section:    { state: true },
    // Source the chat is bound to: 'mobile' (main) or 'project-{id}'.
    _chatSource: { state: true },
    // Label shown in the chat header when inside a project.
    _chatLabel:  { state: true },
    // File shown by the file_viewer section (from `#file_viewer?path=...`).
    _filePath:   { state: true },
  };

  constructor() {
    super();
    this._section    = 'chat';
    this._chatSource = 'mobile';
    this._chatLabel  = '';
    this._filePath   = null;
    // id → name cache, so a cold deep-link (#chat/project-<id> opened by the
    // native shell) can resolve its header label without the project list open.
    this._projectLabels = {};
    // Native shell mode (?native=true): the HTML bottom nav is hidden — a native
    // tab bar drives navigation via location.hash. Mark the host so CSS can drop
    // the safe-area insets the native chrome already provides.
    this._native = new URLSearchParams(location.search).get('native') === 'true';
    if (this._native) this.setAttribute('data-native', '');
  }

  connectedCallback() {
    super.connectedCallback();
    this._onHashChange = () => this._applyHash();
    window.addEventListener('hashchange', this._onHashChange);
    window.addEventListener('popstate',   this._onHashChange);
    // Default route when no hash is present (replaceState: no history entry).
    if (!location.hash) history.replaceState(null, '', '#chat');
    this._applyHash();
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    window.removeEventListener('hashchange', this._onHashChange);
    window.removeEventListener('popstate',   this._onHashChange);
  }

  // ── Hash routing ───────────────────────────────────────────────────────────

  // { section, projectId, filePath } parsed from location.hash. Forms:
  //   #projects                  → section 'projects'
  //   #chat                      → section 'chat' (main mobile session)
  //   #chat/project-<id>         → section 'chat' bound to a project's session
  //   #file_viewer?path=<enc>    → section 'file_viewer' showing a file
  _readHash() {
    const raw = location.hash.slice(1);
    if (!raw) return { section: 'chat', projectId: null, filePath: null };
    // Segment ends at the first `/` (project sub-route) or `?` (query, e.g. the
    // file viewer's `?path=`).
    const cut   = raw.search(/[/?]/);
    const seg   = cut === -1 ? raw : raw.slice(0, cut);
    const section = VALID_SECTIONS.includes(seg) ? seg : 'chat';
    if (section === 'file_viewer') {
      let filePath = null;
      const m = raw.match(/[?&]path=([^&]*)/);
      if (m) { try { filePath = decodeURIComponent(m[1]); } catch { /* keep null */ } }
      return { section, projectId: null, filePath };
    }
    const slash = raw.indexOf('/');
    const sub   = slash === -1 ? '' : raw.slice(slash + 1);
    let projectId = null;
    if (section === 'chat' && sub.startsWith('project-')) {
      projectId = sub.slice('project-'.length) || null;
    }
    return { section, projectId, filePath: null };
  }

  _applyHash() {
    const { section, projectId, filePath } = this._readHash();
    this._section  = section;
    this._filePath = filePath;
    if (projectId) {
      const source = 'project-' + projectId;
      if (this._chatSource !== source) this._chatSource = source;
      this._resolveLabel(projectId);
    } else if (section === 'chat') {
      if (this._chatSource !== 'mobile') this._chatSource = 'mobile';
      this._chatLabel = '';
    }
    // Tell the native shell which section is active. For the file viewer we also
    // pass the document path, so the iOS "Doc" tab can highlight itself and
    // remember the last opened file (re-opened when the user taps Doc again).
    this._notifyNative(
      section,
      projectId ? 'project-' + projectId : null,
      section === 'file_viewer' ? filePath : null,
    );
  }

  // Resolve the display label for a project id (shown in the chat header). Cached
  // in _projectLabels; fetched once from /api/projects when first needed (e.g. a
  // cold native deep-link), then served from cache on every subsequent switch.
  async _resolveLabel(projectId) {
    if (this._projectLabels[projectId] != null) {
      this._chatLabel = this._projectLabels[projectId];
      return;
    }
    try {
      const res = await fetch('/api/projects');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      for (const p of await res.json()) this._projectLabels[p.id] = p.name;
    } catch { /* keep whatever label we have */ }
    this._chatLabel = this._projectLabels[projectId] ?? projectId;
  }

  _nav(section) {
    const target = '#' + section;
    // Setting the hash to the current value fires no event; only change when it
    // differs (also covers exiting a project sub-route via the chat tab).
    if (location.hash !== target) location.hash = target;
  }

  // A project was tapped in the projects list: re-point the chat to its source
  // and push the full project route so back/refresh keep the user in the project.
  _onProjectOpen(e) {
    const { source, label } = e.detail ?? {};
    if (!source || !source.startsWith('project-')) return;
    const id = source.slice('project-'.length);
    if (label) this._projectLabels[id] = label;
    location.hash = '#chat/' + source;
  }

  // Back-out from a project chat: re-point to the main mobile session.
  _onProjectExit() {
    location.hash = '#chat';
  }

  // ── Native bridge ──────────────────────────────────────────────────────────

  // Web → Native: tell the iOS shell which section/project is active so it can
  // highlight the matching native tab. `path` is only set for the file viewer
  // (the document being shown). No-op outside WKWebView — the `skaldNav` message
  // handler only exists when the shell registered it.
  _notifyNative(section, project, path = null) {
    try {
      window.webkit?.messageHandlers?.skaldNav?.postMessage({ section, project, path });
    } catch { /* not in native shell */ }
  }

  render() {
    const s = this._section;
    const item = (id, icon, label, extraClass = '') => html`
      <div class="mobile-nav-item ${extraClass} ${s === id ? 'active' : ''}"
           @click=${() => this._nav(id)}>
        ${id === 'chat'
          ? html`<div class="chat-fab"><i class="bi bi-chat-dots-fill"></i></div>`
          : html`<i class="bi ${icon}"></i>`}
        <span>${label}</span>
      </div>
    `;

    return html`
      <div id="mobile-root">
        <div class="mobile-content">
          <inbox-page
            .visible=${s === 'inbox'}
            style=${s === 'inbox' ? 'flex:1;min-height:0;overflow:hidden' : 'display:none'}
          ></inbox-page>
          <chat-page
            .visible=${s === 'chat'}
            .source=${this._chatSource}
            .label=${this._chatLabel}
            @project-exit=${() => this._onProjectExit()}
            style=${s === 'chat' ? 'flex:1;min-height:0;overflow:hidden;display:flex;flex-direction:column' : 'display:none'}
          ></chat-page>
          <projects-page
            .visible=${s === 'projects'}
            @project-open=${(e) => this._onProjectOpen(e)}
            style=${s === 'projects' ? 'flex:1;min-height:0;overflow:hidden' : 'display:none'}
          ></projects-page>
          <mobile-file-viewer-page
            .visible=${s === 'file_viewer'}
            .path=${this._filePath}
            style=${s === 'file_viewer' ? 'flex:1;min-height:0;overflow:hidden' : 'display:none'}
          ></mobile-file-viewer-page>
          ${['notifications', 'settings'].includes(s) ? html`
            <div class="mobile-coming-soon">
              <i class="bi bi-tools"></i>
              <p>Coming soon</p>
            </div>
          ` : ''}
        </div>

        ${this._native ? nothing : html`
          <nav class="mobile-nav">
            ${item('inbox',         'bi-inbox',         'Inbox')}
            ${item('projects',      'bi-folder2-open',  'Projects')}
            ${item('chat',          '',                 'Chat',    'chat-btn')}
            ${item('notifications', 'bi-bell',          'Alerts')}
            ${item('settings',      'bi-sliders',       'Settings')}
          </nav>
        `}
      </div>
    `;
  }
}

customElements.define('mobile-app', MobileApp);
