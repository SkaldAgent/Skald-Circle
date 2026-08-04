import { html, nothing } from 'lit';
import { ChatSession }   from '../lib/chat-session.js';
import { t, I18nMixin }  from '../lib/i18n.js';
import { renderMsg, renderAttachmentChips } from './copilot-render.js';
import { renderTaskStrip } from './shared/agent-tasks.js';

// Built-in (server-handled) slash commands shown at the top of the composer
// autocomplete. Custom commands (from `commands/<name>/`) are fetched from
// `/api/commands` and appended below.
const SYSTEM_COMMAND_ITEMS = [
  { name: 'help',       description: () => t('copilot.cmd.help') },
  { name: 'clear',      description: () => t('copilot.cmd.clear') },
  { name: 'new',        description: () => t('copilot.cmd.new') },
  { name: 'models',     description: () => t('copilot.cmd.models') },
  { name: 'model',      description: () => t('copilot.cmd.model') },
  { name: 'context',    description: () => t('copilot.cmd.context') },
  { name: 'cost',       description: () => t('copilot.cmd.cost') },
  { name: 'compact',    description: () => t('copilot.cmd.compact') },
  { name: 'resettools', description: () => t('copilot.cmd.resettools') },
  { name: 'sethome',    description: () => t('copilot.cmd.sethome') },
];

// The always-present General tab. It is never stored as an open tab: it exists
// because the copilot exists, and it cannot be closed.
const GENERAL_SOURCE = 'web';

// Which tab is selected is per browser window, so it lives in sessionStorage —
// two windows would otherwise fight over one value, and every tab click would be
// a write. The *set* of open tabs is server-side (`chat_sessions.is_open`), which
// is why it follows the user across devices and a second household member on the
// same browser never sees it.
const ACTIVE_TAB_KEY = 'copilot-active-tab';

// ── The two kinds of tab ──────────────────────────────────────────────────────
//
// A **primary** tab is a source: it shows whatever `web` or `project-7` currently
// points at, which is also where background delivery lands (a notification, a
// finished task, an inbound Telegram message) and what a `/new` moves to a fresh
// conversation. There is at most one per source, and every project's "Open chat"
// lands on it.
//
// A **secondary** tab is one specific conversation, opened with `+`. Its source
// points elsewhere, so it is unreachable by source name and is addressed by id
// throughout — REST, WebSocket and event filtering alike. Nothing is delivered to
// it from the outside; it is a place to work on a second thing at once.
//
// The key is what the selection is stored under and what the render loop tracks,
// so it must stay stable while a tab lives. A primary tab keeps its key across a
// reset (the source is the identity); a secondary tab's key is its session.
const primaryTab   = (source, label, sessionId = null) =>
  ({ key: `src:${source}`, source, sessionId, label, title: null, primary: true });
const secondaryTab = (source, sessionId, label, title = null) =>
  ({ key: `ses:${sessionId}`, source, sessionId, label, title, primary: false });

export class AppCopilot extends I18nMixin(ChatSession) {
  static properties = {
    _collapsed:     { state: true },
    _mode:          { state: true },
    _me:            { state: true },
    _modelOpen:     { state: true },
    _groupOpen:     { state: true },
    _tabs:          { state: true },
    _activeSource:  { state: true },
    _activeSessionId: { state: true },
    _newTabOpen:    { state: true },
    _newTabTargets: { state: true },
    _newTabAnchor:  { state: true },
    _renamingKey:   { state: true },
    _cmdMenu:       { state: true },
    _cmdSel:        { state: true },
  };

  constructor() {
    super();
    this._collapsed     = false;
    // 'full' fills the workspace (home route), 'dock' is the side panel.
    this._mode          = 'dock';
    this._me            = null;
    this._modelOpen     = false;
    this._groupOpen     = false;
    this._resizing      = false;
    // Slash-command autocomplete: `_cmdMenu` is the filtered list currently shown
    // (null = hidden), `_cmdSel` the highlighted index, `_allCommands` the merged
    // system + custom list fetched once from `/api/commands`.
    this._cmdMenu       = null;
    this._cmdSel        = 0;
    this._allCommands   = null;
    // Browser-style tabs. Two kinds, and the difference is which conversation they
    // name — see `TAB` below. 'General' is always present and not closable.
    this._tabs          = [primaryTab(GENERAL_SOURCE, t('chat.tab.general'))];
    // The `+` menu: null when closed, otherwise the list of things a new chat can
    // be started on (General + the caller's projects), fetched on first open.
    this._newTabOpen    = false;
    this._newTabTargets = null;
    this._newTabAnchor  = null;
    // Key of the tab being renamed inline, if any.
    this._renamingKey   = null;
    this._onResizeMove  = this._onResizeMove.bind(this);
    this._onResizeUp    = this._onResizeUp.bind(this);
    this._onKeydown     = this._onKeydown.bind(this);
    this._onKeyup       = this._onKeyup.bind(this);
    this._onProjectChatOpen = this._onProjectChatOpen.bind(this);
    this._onCopilotOpen     = this._onCopilotOpen.bind(this);
    this._onPageChange      = this._onPageChange.bind(this);
  }

  // The desktop shell routes `#session/{id}`, so a background task's row links
  // through to what it is doing.
  get _canOpenTaskSession() { return true; }

  connectedCallback() {
    // Before super: the base loads history and opens the WS for `_source` in its
    // own connectedCallback, so the restored selection has to be in place or the
    // first paint fetches General and then immediately throws it away.
    // sessionStorage is synchronous, which is what makes this possible; the tab
    // *set* arrives over the network and reconciles in `_restoreTabs`.
    const active = sessionStorage.getItem(ACTIVE_TAB_KEY) ?? '';
    if (active.startsWith('ses:')) {
      this._activeSessionId = Number(active.slice(4)) || null;
    } else if (active.startsWith('src:') && active !== `src:${GENERAL_SOURCE}`) {
      this._activeSource = active.slice(4);
    }
    // The base's is async and owns the first WS: hand it to `_restoreTabs`, which
    // must not switch source while that connection is still being set up.
    const ready = super.connectedCallback?.();
    this._restoreState();
    this._restoreTabs(ready);
    this._loadCommands();
    this._loadMe();
    this._loadSecurityGroups();
    // Same element, two layouts: the chat is the home page ('full') and docks
    // to the side on every other route — state is never lost, it only resizes.
    this._applyMode(this._pageFromHash() === 'home' ? 'full' : 'dock');
    window.addEventListener('keydown',           this._onKeydown);
    window.addEventListener('keyup',             this._onKeyup);
    window.addEventListener('project-chat-open', this._onProjectChatOpen);
    window.addEventListener('copilot-open',      this._onCopilotOpen);
    window.addEventListener('llm-page-change',   this._onPageChange);
  }

  _pageFromHash() {
    const m = location.hash.slice(1).match(/^([^/?]+)/);
    const seg = m ? m[1] : '';
    const known = ['inbox', 'dashboard', 'tasks', 'projects', 'models', 'providers', 'approval', 'agents', 'users', 'roles', 'connectors', 'connector', 'marketplace', 'profile', 'config', 'llm-requests', 'session', 'system-agents', 'file_viewer', 'tool_detail'];
    return known.includes(seg) ? seg : 'home';
  }

  _onPageChange(e) {
    this._applyMode(e.detail?.page === 'home' ? 'full' : 'dock');
  }

  _applyMode(mode) {
    if (mode === this._mode && this.getAttribute('mode') === mode) return;
    this._mode = mode;
    this.setAttribute('mode', mode);
  }

  async _loadMe() {
    try {
      const res = await fetch('/api/auth/me');
      if (res.ok) this._me = await res.json();
    } catch { /* ignore */ }
  }

  _restoreState() {
    const w = localStorage.getItem('copilot-width');
    if (w) document.documentElement.style.setProperty('--copilot-width', w);
    if (localStorage.getItem('copilot-collapsed') === 'true') {
      this._setCollapsed(true);
    }
  }

  disconnectedCallback() {
    super.disconnectedCallback?.();
    window.removeEventListener('keydown',           this._onKeydown);
    window.removeEventListener('keyup',             this._onKeyup);
    window.removeEventListener('project-chat-open', this._onProjectChatOpen);
    window.removeEventListener('copilot-open',      this._onCopilotOpen);
    window.removeEventListener('llm-page-change',   this._onPageChange);
  }

  _onCopilotOpen() {
    this._setCollapsed(false);
  }

  _setCollapsed(value) {
    this._collapsed = value;
    this.classList.toggle('collapsed', value);
    localStorage.setItem('copilot-collapsed', value);
    window.dispatchEvent(new CustomEvent('copilot-collapsed', { detail: { collapsed: value } }));
  }

  // ── Tabs ────────────────────────────────────────────────────────────────────

  // The tab this chat is currently bound to.
  get _activeKey() {
    return this._activeSessionId ? `ses:${this._activeSessionId}` : `src:${this._source}`;
  }

  // Restore the tabs the user left open. They come from their own (encrypted)
  // database rather than this browser, so the bar is the same on every device and
  // a shared laptop never mixes two members' tabs.
  async _restoreTabs(ready) {
    // Switching tabs tears down the WS, so it has to wait for the one the base
    // opens on mount — otherwise both run and the connection is left doubled.
    const settled = Promise.resolve(ready).catch(() => {});
    let rows = [];
    try {
      const res = await fetch('/api/sessions/open');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      rows = await res.json();
    } catch (e) {
      console.error('Failed to restore copilot tabs:', e);
      // The restored selection can't be trusted without the set that justifies it.
      await settled;
      await this._fallBackToGeneral();
      return;
    }

    // Merged, not replaced: the user may already have opened a tab while this was
    // in flight, and it would not be in a response the server built before it.
    const merged = [...this._tabs];
    for (const row of rows) {
      const tab = row.primary
        ? primaryTab(row.source, row.label || row.source, row.session_id)
        : secondaryTab(row.source, row.session_id, row.label || row.source, row.title);
      const known = merged.find(t => t.key === tab.key);
      if (known) { known.sessionId ??= tab.sessionId; continue; }
      merged.push(tab);
    }
    this._tabs = merged;

    // The selection is per window and the set is per user, so they can disagree:
    // another window may have closed the tab this one had selected.
    await settled;
    await this._fallBackToGeneral();
  }

  // Land on General when the bound tab is not (or no longer) in the bar.
  async _fallBackToGeneral() {
    if (this._tabs.some(t => t.key === this._activeKey)) return;
    this._selectTab(`src:${GENERAL_SOURCE}`);
  }

  // A project chat was opened elsewhere (the board, the sidebar): show its primary
  // tab, expand the copilot, and switch the live connection to it. Deliberately
  // never opens a second conversation — "Open chat" resumes the project's own.
  _onProjectChatOpen(e) {
    const { source, label, session_id } = e.detail ?? {};
    if (!source) return;
    const key   = `src:${source}`;
    const known = this._tabs.find(t => t.key === key);
    if (known) {
      // Keep the id fresh — the session behind a source changes on every reset.
      this._bindTabSession(known, session_id);
    } else {
      this._tabs = [...this._tabs, primaryTab(source, label || source, session_id)];
      this._persistTab(session_id, true);
    }
    this._setCollapsed(false);
    this._selectTab(key);
  }

  _selectTab(key) {
    const tab = this._tabs.find(t => t.key === key);
    if (!tab) return;
    try { sessionStorage.setItem(ACTIVE_TAB_KEY, key); } catch { /* private mode */ }
    if (key === this._activeKey) return;
    // A primary tab is addressed by source so it keeps following resets; a
    // secondary one by id, because its source points at a different conversation.
    this._switchTo(tab.source, tab.primary ? null : tab.sessionId);
  }

  // Close a tab. The conversation itself is untouched — closing only clears its
  // `is_open` flag, and a project's chat comes back from the board with all its
  // history. The General tab is never closable and is not a stored tab at all.
  _closeTab(key, e) {
    e?.stopPropagation();
    if (key === `src:${GENERAL_SOURCE}`) return;
    const tab = this._tabs.find(t => t.key === key);
    if (!tab) return;
    const wasActive = key === this._activeKey;
    this._tabs = this._tabs.filter(t => t.key !== key);
    this._persistTab(tab.sessionId, false);
    if (wasActive) this._selectTab(`src:${GENERAL_SOURCE}`);
  }

  // This chat became a different conversation: a primary tab was reset, or a
  // secondary one started over. `is_open` hangs on the row, so it has to be moved
  // or the tab would close itself out from under the user at the next login.
  _onSessionReplaced(source, sessionId, previous) {
    const key = previous ? `ses:${previous}` : `src:${source}`;
    const tab = this._tabs.find(t => t.key === key);
    if (!tab) return;
    if (tab.primary) { this._bindTabSession(tab, sessionId); return; }
    // A secondary tab *is* its session, so starting over replaces the tab.
    const fresh = secondaryTab(tab.source, sessionId, tab.label, null);
    this._tabs = this._tabs.map(t => (t.key === key ? fresh : t));
    this._persistTab(previous, false);
    this._persistTab(sessionId, true);
    try { sessionStorage.setItem(ACTIVE_TAB_KEY, fresh.key); } catch { /* private mode */ }
  }

  // Point a primary tab at the session it now shows. The previous one is closed in
  // the same breath: leaving it open would have the source restore twice, and the
  // stale row would be the one a later close cleared.
  //
  // General is the exception: it is never a stored tab, so marking its rows open
  // would leave a trail of flags the bar deliberately ignores and nothing clears.
  _bindTabSession(tab, sessionId) {
    if (!sessionId || tab.sessionId === sessionId) return;
    const previous = tab.sessionId;
    tab.sessionId  = sessionId;
    if (tab.key === `src:${GENERAL_SOURCE}`) return;
    if (previous) this._persistTab(previous, false);
    this._persistTab(sessionId, true);
  }

  // Double-click renames — the affordance every tabbed interface already has, and
  // it keeps the bar free of a per-tab edit button.
  _renderTab(tab) {
    const label = this._tabLabel(tab);
    return html`
      <div
        class="copilot-tab ${tab.key === this._activeKey ? 'copilot-tab--active' : ''}"
        @click=${() => this._selectTab(tab.key)}
        @dblclick=${e => this._startRename(tab.key, e)}
        title=${label}
      >
        ${this._renamingKey === tab.key ? html`
          <input
            class="copilot-tab-rename"
            .value=${tab.title ?? ''}
            placeholder=${label}
            @click=${e => e.stopPropagation()}
            @keydown=${e => this._onRenameKey(tab.key, e)}
            @blur=${e => this._commitRename(tab.key, e.target.value)}
          >
        ` : html`
          <span class="copilot-tab-label">${label}</span>
          ${tab.key !== `src:${GENERAL_SOURCE}` ? html`
            <button class="copilot-tab-close" title=${t('chat.close_tab')}
              @click=${e => this._closeTab(tab.key, e)}>
              <i class="bi bi-x"></i>
            </button>
          ` : nothing}
        `}
      </div>
    `;
  }

  // ── The `+` menu ────────────────────────────────────────────────────────────

  async _toggleNewTab(e) {
    this._newTabOpen = !this._newTabOpen;
    if (this._newTabOpen && e) {
      const r = e.currentTarget.getBoundingClientRect();
      this._newTabAnchor = { top: r.bottom + 4, left: r.left };
    }
    if (!this._newTabOpen || this._newTabTargets) return;
    // General plus the caller's projects — the two things a chat can be *about*.
    // A project entry starts a second conversation there, with the coordinator
    // agent and the project's context, exactly like its own tab.
    let projects = [];
    try {
      const res = await fetch('/api/projects');
      if (res.ok) projects = await res.json();
    } catch { /* the General entry is still useful */ }
    this._newTabTargets = [
      { source: GENERAL_SOURCE, label: t('chat.tab.general') },
      ...projects.map(p => ({ source: `project-${p.id}`, label: p.name })),
    ];
  }

  async _openNewTab(target) {
    this._newTabOpen = false;
    try {
      const res = await fetch(
        `/api/sessions/new?source=${encodeURIComponent(target.source)}`, { method: 'POST' });
      if (!res.ok) throw new Error(await res.text());
      const row = await res.json();
      const tab = secondaryTab(row.source, row.session_id, row.label || target.label, null);
      this._tabs = [...this._tabs, tab];
      this._setCollapsed(false);
      this._selectTab(tab.key);
    } catch (e) {
      this._pushError('Could not open a new chat: ' + e.message);
    }
  }

  // ── Renaming ────────────────────────────────────────────────────────────────

  _startRename(key, e) {
    e?.stopPropagation();
    this._renamingKey = key;
    this.updateComplete.then(() => {
      const input = this.querySelector('.copilot-tab-rename');
      input?.focus();
      input?.select();
    });
  }

  _onRenameKey(key, e) {
    if (e.key === 'Enter')  { e.preventDefault(); this._commitRename(key, e.target.value); }
    if (e.key === 'Escape') { e.preventDefault(); this._renamingKey = null; }
  }

  // An empty name clears the title, which gives back the automatic label rather
  // than a blank tab — so the box is also the way to undo a rename.
  async _commitRename(key, value) {
    this._renamingKey = null;
    const tab = this._tabs.find(t => t.key === key);
    if (!tab?.sessionId) return;
    const title = value.trim();
    if ((tab.title ?? '') === title) return;
    tab.title = title || null;
    this.requestUpdate();
    try {
      const res = await fetch(`/api/sessions/${tab.sessionId}/title`, {
        method:  'PUT',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({ title: title || null }),
      });
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
    } catch (e) {
      console.error('Failed to rename the chat:', e);
    }
  }

  // What a tab prints. A user-set title always wins. Without one, secondary tabs
  // on the same source would all read "General" — so they are numbered by their
  // position among their siblings, which is stable and needs no extra state.
  _tabLabel(tab) {
    if (tab.title) return tab.title;
    if (tab.primary) return tab.label;
    const siblings = this._tabs.filter(t => !t.primary && t.source === tab.source);
    return `${tab.label} ${siblings.indexOf(tab) + 2}`;
  }

  // Best-effort: a tab that failed to persist reappears (or lingers) at the next
  // login, which is a nuisance, never a loss.
  async _persistTab(sessionId, open) {
    if (!sessionId) return;
    try {
      await fetch(`/api/sessions/${sessionId}/open`, {
        method:  'PUT',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({ open }),
      });
    } catch (e) {
      console.error('Failed to persist copilot tab:', e);
    }
  }

  // ── DOM hooks ─────────────────────────────────────────────────────────────────

  _inputEl() {
    return this.querySelector('.copilot-textarea');
  }

  _messagesContainer() {
    return this.querySelector('.copilot-messages');
  }

  _onMessagePushed(item) {
    if (item.kind === 'pending_write') {
      this.updateComplete.then(() => {
        const panels = this.querySelectorAll('.copilot-approval');
        const el = panels[panels.length - 1];
        if (el) el.scrollIntoView({ behavior: 'smooth', block: 'nearest' });
      });
    } else {
      this._scrollToBottom();
    }
  }

  // ── Resize ────────────────────────────────────────────────────────────────────

  _startResize(e) {
    this._resizing     = true;
    this._resizeStartX = e.clientX;
    this._resizeStartW = this.offsetWidth;
    window.addEventListener('mousemove', this._onResizeMove);
    window.addEventListener('mouseup',   this._onResizeUp);
    e.preventDefault();
  }

  _onResizeMove(e) {
    if (!this._resizing) return;
    const delta    = this._resizeStartX - e.clientX;
    const newWidth = Math.max(260, Math.min(720, this._resizeStartW + delta));
    document.documentElement.style.setProperty('--copilot-width', `${newWidth}px`);
  }

  _onResizeUp() {
    this._resizing = false;
    window.removeEventListener('mousemove', this._onResizeMove);
    window.removeEventListener('mouseup',   this._onResizeUp);
    const w = getComputedStyle(document.documentElement).getPropertyValue('--copilot-width').trim();
    if (w) localStorage.setItem('copilot-width', w);
  }

  // ── Input ─────────────────────────────────────────────────────────────────────

  _handleKeydown(e) {
    if (e.key === 'Enter' && !e.shiftKey) {
      e.preventDefault();
      this._send();
    }
  }

  // ── Slash-command autocomplete ────────────────────────────────────────────────

  /** Fetch custom commands once and merge them below the built-in system ones. */
  async _loadCommands() {
    try {
      const res    = await fetch('/api/commands');
      const custom = res.ok ? await res.json() : [];
      this._allCommands = [...SYSTEM_COMMAND_ITEMS, ...custom];
    } catch {
      this._allCommands = [...SYSTEM_COMMAND_ITEMS];
    }
  }

  /** Recompute the menu from the current input value. Shown only while typing the
   *  command name (a leading `/` with no whitespace yet); hidden once args start. */
  _updateCmdMenu(value) {
    const m = /^\/([a-z0-9_-]*)$/i.exec(value);
    if (!m) { if (this._cmdMenu) this._cmdMenu = null; return; }
    const prefix = m[1].toLowerCase();
    const items  = (this._allCommands || SYSTEM_COMMAND_ITEMS)
      .filter(c => c.name.toLowerCase().startsWith(prefix));
    this._cmdMenu = items.length ? items : null;
    this._cmdSel  = 0;
  }

  /** Insert the chosen command (`/name `) and close the menu, ready for arguments. */
  _applyCmd(name) {
    const el = this._inputEl();
    if (el) {
      el.value = `/${name} `;
      el.focus();
      this._autoResize(el);
    }
    this._cmdMenu = null;
  }

  /** Composer keydown: drive the menu when open, else fall back to send-on-Enter. */
  _composerKeydown(e) {
    const menu = this._cmdMenu;
    if (menu && menu.length) {
      if (e.key === 'ArrowDown') { e.preventDefault(); this._cmdSel = (this._cmdSel + 1) % menu.length; return; }
      if (e.key === 'ArrowUp')   { e.preventDefault(); this._cmdSel = (this._cmdSel - 1 + menu.length) % menu.length; return; }
      if (e.key === 'Enter' || e.key === 'Tab') { e.preventDefault(); this._applyCmd(menu[this._cmdSel].name); return; }
      if (e.key === 'Escape')    { e.preventDefault(); this._cmdMenu = null; return; }
    }
    this._handleKeydown(e);
  }

  // ── Ctrl+Space push-to-talk shortcut (desktop only) ──────────────────────────
  // Voice recording + transcription is owned by the ChatSession base class; the
  // only desktop-specific bit is the global Ctrl+Space hold-to-record shortcut.

  _onKeydown(e) {
    if (!this._hasTranscribe) return;
    if (e.code === 'Space' && e.ctrlKey && !e.repeat) {
      e.preventDefault();
      if (!this._recording) this._startRecording(true);
    }
  }

  _onKeyup(e) {
    if (!this._hasTranscribe) return;
    if (e.code === 'Space' && this._recording && this._shortcutRecording) {
      e.preventDefault();
      this._stopRecording();
    }
  }

  // ── Render helpers ────────────────────────────────────────────────────────────

  _toggleExpand(id) {
    const next = new Set(this._expanded);
    if (next.has(id)) next.delete(id); else next.add(id);
    this._expanded = next;
  }

  // ── Render ────────────────────────────────────────────────────────────────────

  _sendSuggestion(text) {
    const el = this._inputEl();
    if (!el) return;
    el.value = text;
    this._send();
  }

  _renderEmptyState() {
    // Dock mode keeps the compact greeting bubble; full mode (home) shows the
    // welcome hero with a few prompt suggestions to get a conversation going.
    if (this._mode !== 'full') {
      return html`<div class="copilot-msg assistant">${t('chat.hello')}</div>`;
    }
    const name = this._me?.display_name || this._me?.username;
    const suggestions = [
      { icon: 'bi-stars',          text: t('chat.suggest.1') },
      { icon: 'bi-calendar-check', text: t('chat.suggest.2') },
      { icon: 'bi-book',           text: t('chat.suggest.3') },
      { icon: 'bi-heart',          text: t('chat.suggest.4') },
    ];
    return html`
      <div class="chat-hero">
        <img class="chat-hero-logo" src="/assets/icons/icon-192.png" alt="" />
        <h1 class="chat-hero-title">${name ? t('chat.greeting.named', { name }) : t('chat.greeting')}</h1>
        <p class="chat-hero-sub">${t('chat.greeting.sub')}</p>
        <div class="chat-suggestions">
          ${suggestions.map(s => html`
            <button class="chat-suggestion" @click=${() => this._sendSuggestion(s.text)}>
              <i class="bi ${s.icon}"></i>
              <span>${s.text}</span>
            </button>
          `)}
        </div>
      </div>
    `;
  }

  render() {
    // Collapse applies to the dock only: on the home route the chat IS the page.
    if (this._collapsed && this._mode !== 'full') return nothing;
    const full = this._mode === 'full';

    return html`
      ${!full ? html`
        <div class="copilot-resize-handle" @mousedown=${(e) => this._startResize(e)}></div>
      ` : nothing}

      <div class="copilot-header">
        <i class="bi bi-stars"></i>
        <span>${t('chat.title')}</span>
        <span class="chat-privacy" title=${t('chat.privacy.hint')}>
          <i class="bi bi-lock-fill"></i>${t('chat.privacy')}
        </span>
        ${!full ? html`
          <button
            class="btn btn-sm btn-outline-secondary ms-auto copilot-collapse-btn"
            title=${t('chat.collapse')}
            @click=${() => { this._setCollapsed(true); }}
          >
            <i class="bi bi-chevron-right"></i>
          </button>
        ` : nothing}
      </div>

      <div class="copilot-tabs">
        ${this._tabs.map(tab => this._renderTab(tab))}
        <div class="copilot-tab-new">
          <button class="copilot-tab-add" title=${t('chat.new_tab')}
            @click=${(e) => this._toggleNewTab(e)}>
            <i class="bi bi-plus-lg"></i>
          </button>
          ${this._newTabOpen ? html`
            <div class="copilot-model-overlay" @click=${() => { this._newTabOpen = false; }}></div>
            <div class="copilot-tab-menu"
              style=${this._newTabAnchor ? `top:${this._newTabAnchor.top}px;left:${this._newTabAnchor.left}px` : ''}>
              ${this._newTabTargets === null
                ? html`<div class="copilot-tab-menu-empty">${t('chat.new_tab.loading')}</div>`
                : this._newTabTargets.map(target => html`
                    <button class="copilot-tab-menu-item"
                      @click=${() => this._openNewTab(target)}>${target.label}</button>
                  `)}
            </div>
          ` : nothing}
        </div>
      </div>

      <div class="copilot-messages">
        ${this._messages.length === 0
          ? this._renderEmptyState()
          : this._messages.map(m => renderMsg(this, m))}

        ${this._waiting ? html`
          <div class="copilot-msg assistant copilot-thinking">
            <span class="spinner-border spinner-border-sm me-2" role="status"></span>
            ${t('chat.thinking')}
          </div>
        ` : nothing}

        ${this._showJump ? html`
          <button class="copilot-jump-btn" type="button"
                  title=${t('chat.scroll_to_latest')}
                  aria-label=${t('chat.scroll_to_latest')}
                  @click=${() => this._jumpToBottom()}>
            <i class="bi bi-arrow-down-circle-fill"></i>
          </button>
        ` : nothing}
      </div>

      <div class="copilot-input-area">
        ${renderTaskStrip(this)}
        ${this._renderNoModelsBanner()}
        <div class="copilot-composer"
             @dragover=${(e) => e.preventDefault()}
             @drop=${(e) => this._onDrop(e)}>
          ${this._cmdMenu?.length ? html`
            <div class="copilot-cmd-menu">
              ${this._cmdMenu.map((c, i) => html`
                <button
                  class="copilot-cmd-item ${i === this._cmdSel ? 'active' : ''}"
                  @mousedown=${(e) => { e.preventDefault(); this._applyCmd(c.name); }}
                >
                  <span class="copilot-cmd-name">/${c.name}</span>
                  <span class="copilot-cmd-desc">${typeof c.description === 'function' ? c.description() : c.description}</span>
                </button>
              `)}
            </div>
          ` : nothing}
          ${renderAttachmentChips(this, this._attachments, { removable: true })}
          <input
            type="file"
            multiple
            class="copilot-file-input"
            style="display:none"
            @change=${(e) => { this._addFiles(e.target.files); e.target.value = ''; }}
          />
          <textarea
            class="copilot-textarea"
            rows="1"
            ?disabled=${this._noModels}
            placeholder=${t('chat.placeholder')}
            @keydown=${this._composerKeydown}
            @input=${(e) => { this._autoResize(e.target); this._updateCmdMenu(e.target.value); }}
            @paste=${(e) => this._onPaste(e)}
          ></textarea>
          <div class="copilot-toolbar">
            <div class="copilot-toolbar-left">
              <button
                class="copilot-toolbar-btn"
                title=${t('chat.attach')}
                @click=${() => this.querySelector('.copilot-file-input')?.click()}
              ><i class="bi bi-paperclip"></i></button>
              ${this._providers.length > 1 ? html`
                <div class="copilot-model-wrap">
                  ${this._modelOpen ? html`
                    <div class="copilot-model-overlay" @click=${() => { this._modelOpen = false; }}></div>
                    <div class="copilot-model-dropdown">
                      ${this._providers.map(p => html`
                        <button
                          class="copilot-model-item ${p === this._selectedClient ? 'active' : ''}"
                          @click=${() => { this._selectClient(p); this._modelOpen = false; }}
                        >${p}</button>
                      `)}
                    </div>
                  ` : nothing}
                  <button class="copilot-model-pill" @click=${() => { this._modelOpen = !this._modelOpen; }}>
                    <i class="bi bi-stars"></i>
                    <span>${this._selectedClient ?? 'auto'}</span>
                    <i class="bi bi-chevron-${this._modelOpen ? 'down' : 'up'}"></i>
                  </button>
                </div>
              ` : nothing}
              ${this._securityGroups.length > 1 ? html`
                <div class="copilot-model-wrap">
                  ${this._groupOpen ? html`
                    <div class="copilot-model-overlay" @click=${() => { this._groupOpen = false; }}></div>
                    <div class="copilot-model-dropdown">
                      ${this._securityGroups.map(g => html`
                        <button
                          class="copilot-model-item ${g.id === this._selectedGroup ? 'active' : ''}"
                          @click=${() => { this._selectGroup(g.id); this._groupOpen = false; }}
                        >${g.name}</button>
                      `)}
                    </div>
                  ` : nothing}
                  <button
                    class="copilot-model-pill"
                    title=${t('chat.security_group')}
                    @click=${() => { this._groupOpen = !this._groupOpen; }}>
                    <i class="bi bi-shield-lock"></i>
                    <span>${this._securityGroups.find(g => g.id === this._selectedGroup)?.name ?? this._selectedGroup}</span>
                    <i class="bi bi-chevron-${this._groupOpen ? 'down' : 'up'}"></i>
                  </button>
                </div>
              ` : nothing}
              <button
                class="copilot-toolbar-btn"
                title=${t('chat.new_session')}
                @click=${() => this._startNewSession()}
              ><i class="bi bi-trash"></i></button>
            </div>
            <div class="copilot-toolbar-right">
              ${this._hasTranscribe ? html`
                <button
                  class="copilot-send-btn ${this._recording ? 'copilot-send-btn--recording' : ''}"
                  title="${this._recording ? t('chat.stop') : t('chat.attach')}"
                  @click=${() => this._toggleRecording()}
                >
                  <i class="bi ${this._recording ? 'bi-stop-circle-fill' : 'bi-mic-fill'}"></i>
                </button>
              ` : nothing}
              ${this._waiting
                ? html`<button class="copilot-send-btn copilot-send-btn--stop" @click=${() => this._cancel()} title=${t('chat.stop')}>
                    <i class="bi bi-stop-fill"></i>
                  </button>`
                : nothing}
              <button class="copilot-send-btn" ?disabled=${this._noModels} @click=${() => this._send()} title=${t('chat.send')}>
                <i class="bi bi-send-fill"></i>
              </button>
            </div>
          </div>
        </div>
      </div>
    `;
  }
}
