import { html, nothing } from 'lit';
import { ChatSession }   from '../lib/chat-session.js';
import { t, I18nMixin }  from '../lib/i18n.js';
import { renderMsg, renderAttachmentChips } from './copilot-render.js';

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

export class AppCopilot extends I18nMixin(ChatSession) {
  static properties = {
    _collapsed:     { state: true },
    _mode:          { state: true },
    _me:            { state: true },
    _modelOpen:     { state: true },
    _tabs:          { state: true },
    _activeSource:  { state: true },
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
    this._resizing      = false;
    // Slash-command autocomplete: `_cmdMenu` is the filtered list currently shown
    // (null = hidden), `_cmdSel` the highlighted index, `_allCommands` the merged
    // system + custom list fetched once from `/api/commands`.
    this._cmdMenu       = null;
    this._cmdSel        = 0;
    this._allCommands   = null;
    // Browser-style tabs: 'General' (the default 'web' source) is always present and
    // not closable; project chats are added on demand and addressed by their source.
    this._tabs          = [{ source: 'web', label: t('chat.tab.general') }];
    this._onResizeMove  = this._onResizeMove.bind(this);
    this._onResizeUp    = this._onResizeUp.bind(this);
    this._onKeydown     = this._onKeydown.bind(this);
    this._onKeyup       = this._onKeyup.bind(this);
    this._onProjectChatOpen = this._onProjectChatOpen.bind(this);
    this._onCopilotOpen     = this._onCopilotOpen.bind(this);
    this._onPageChange      = this._onPageChange.bind(this);
  }

  connectedCallback() {
    super.connectedCallback?.();
    this._restoreState();
    this._loadCommands();
    this._loadMe();
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
    const known = ['inbox', 'dashboard', 'tasks', 'projects', 'models', 'providers', 'approval', 'agents', 'users', 'roles', 'connectors', 'connector', 'catalog', 'marketplace', 'profile', 'config', 'llm-requests', 'session', 'tic', 'file_viewer'];
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

  // A project chat was opened elsewhere (e.g. the project board): add its tab if
  // new, expand the copilot, and switch the live connection to it.
  _onProjectChatOpen(e) {
    const { source, label } = e.detail ?? {};
    if (!source) return;
    if (!this._tabs.some(t => t.source === source)) {
      this._tabs = [...this._tabs, { source, label: label || source }];
    }
    this._setCollapsed(false);
    this._selectTab(source);
  }

  _selectTab(source) {
    if (source === this._source) return;
    this._switchSource(source);   // base: tear down WS, reload history, reconnect
  }

  // Close a project tab (UI only — the session persists server-side and can be
  // reopened from the board). The 'web'/General tab is never closable.
  _closeTab(source, e) {
    e?.stopPropagation();
    if (source === 'web') return;
    const wasActive = source === this._source;
    this._tabs = this._tabs.filter(t => t.source !== source);
    if (wasActive) this._switchSource('web');
  }

  // ── DOM hooks ─────────────────────────────────────────────────────────────────

  _inputEl() {
    return this.querySelector('.copilot-textarea');
  }

  _scrollToBottom() {
    this.updateComplete.then(() => {
      const el = this.querySelector('.copilot-messages');
      if (el) el.scrollTop = el.scrollHeight;
    });
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

      ${this._tabs.length > 1 ? html`
        <div class="copilot-tabs">
          ${this._tabs.map(tab => html`
            <div
              class="copilot-tab ${tab.source === this._source ? 'copilot-tab--active' : ''}"
              @click=${() => this._selectTab(tab.source)}
              title=${tab.label}
            >
              <span class="copilot-tab-label">${tab.label}</span>
              ${tab.source !== 'web' ? html`
                <button class="copilot-tab-close" title=${t('chat.close_tab')}
                  @click=${e => this._closeTab(tab.source, e)}>
                  <i class="bi bi-x"></i>
                </button>
              ` : nothing}
            </div>
          `)}
        </div>
      ` : nothing}

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
      </div>

      <div class="copilot-input-area">
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
              <button class="copilot-send-btn" @click=${() => this._send()} title=${t('chat.send')}>
                <i class="bi bi-send-fill"></i>
              </button>
            </div>
          </div>
        </div>
      </div>
    `;
  }
}
