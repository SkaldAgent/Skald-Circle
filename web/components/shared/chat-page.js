import { html, nothing } from 'lit';
import { ChatSession }   from '../../lib/chat-session.js';
import { t }             from '../../lib/i18n.js';
import { renderMsg, renderAttachmentChips } from '../copilot-render.js';
import { renderTaskStrip } from './agent-tasks.js';

export class ChatPage extends ChatSession {
  static properties = {
    visible: { type: Boolean },
    // Target source. Defaults to the main mobile session; set to `project-{id}`
    // to bind this chat to a project's coordinator session.
    source:  { type: String },
    // Human-readable label for the active source (e.g. the project name), shown
    // in the header when inside a project.
    label:   { type: String },
    _me:     { state: true },
  };

  constructor() {
    super();
    this.visible = false;
    this.source  = 'mobile';
    this.label   = '';
    this._me     = null;
  }

  async _loadMe() {
    try {
      const res = await fetch('/api/auth/me');
      if (res.ok) this._me = await res.json();
    } catch { /* greeting falls back to the generic one */ }
  }

  connectedCallback() {
    // Honour the initial `source` prop on the first connect so a cold deep-link
    // (e.g. the native shell opening #chat/project-<id>) connects straight to it,
    // instead of connecting to the 'mobile' default and switching a tick later
    // (which would briefly open two WebSockets). Later `source` prop changes are
    // still handled by `updated` below.
    if (this.source && this.source !== this._wsSource) this._activeSource = this.source;
    super.connectedCallback();
    this._loadMe();
  }

  updated(changed) {
    if (changed.has('visible') && this.visible) {
      this._forceScrollToBottom();
    }
    // The owner (mobile-app) re-points this chat by changing `source`. Switch the
    // live connection — base `_switchSource` tears down the WS, reloads that
    // source's history, and reconnects. The guard skips the initial no-op render.
    if (changed.has('source') && this.source !== this._source) {
      this._switchSource(this.source);
    }
  }

  // ── Source identity ────────────────────────────────────────────────────────

  // Static fallback used only before the first `source` prop is applied.
  get _wsSource() { return 'mobile'; }

  get _inProject() {
    return typeof this.source === 'string' && this.source.startsWith('project-');
  }

  _exitProject() {
    this.dispatchEvent(new CustomEvent('project-exit', { bubbles: true, composed: true }));
  }

  // ── DOM hooks ──────────────────────────────────────────────────────────────

  _inputEl() {
    return this.querySelector('.chat-page-textarea');
  }

  _messagesContainer() {
    return this.querySelector('.chat-page-messages');
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

  // ── Input ──────────────────────────────────────────────────────────────────
  // Note: unlike the desktop copilot, Enter does NOT send here. On mobile there
  // is no practical Shift+Enter, so Enter inserts a newline (the textarea's
  // default) and the explicit send button is the only way to submit — making
  // multi-line messages possible.

  // ── Toggle expand ──────────────────────────────────────────────────────────

  _toggleExpand(id) {
    const next = new Set(this._expanded);
    if (next.has(id)) next.delete(id); else next.add(id);
    this._expanded = next;
  }

  _sendSuggestion(text) {
    const el = this._inputEl();
    if (!el) return;
    el.value = text;
    this._send();
  }

  // Welcome hero (main session) with a few prompt suggestions, mirroring the
  // desktop home. Inside a project the empty state stays compact — the header
  // already carries the context.
  _renderEmptyState() {
    if (this._inProject) {
      return html`
        <div class="chat-page-hero">
          <i class="bi bi-folder2-open" style="font-size:1.6rem;color:var(--accent)"></i>
          <p class="chat-page-hero-sub" style="margin:0.5rem 0 0">${this.label || t('chat.mobile.project')}</p>
        </div>
      `;
    }
    const name = this._me?.display_name || this._me?.username;
    const suggestions = [
      { icon: 'bi-stars',          text: t('chat.suggest.1') },
      { icon: 'bi-calendar-check', text: t('chat.suggest.2') },
      { icon: 'bi-book',           text: t('chat.suggest.3') },
      { icon: 'bi-heart',          text: t('chat.suggest.4') },
    ];
    return html`
      <div class="chat-page-hero">
        <img class="chat-page-hero-logo" src="/assets/icons/icon-192.png" alt="" />
        <h1 class="chat-page-hero-title">${name ? t('chat.greeting.named', { name }) : t('chat.greeting')}</h1>
        <p class="chat-page-hero-sub">${t('chat.greeting.sub')}</p>
        <div class="chat-page-suggestions">
          ${suggestions.map(s => html`
            <button class="chat-page-suggestion" @click=${() => this._sendSuggestion(s.text)}>
              <i class="bi ${s.icon}"></i>
              <span>${s.text}</span>
            </button>
          `)}
        </div>
      </div>
    `;
  }

  // ── Render ─────────────────────────────────────────────────────────────────

  render() {
    if (!this.visible) return nothing;

    return html`
      <div class="chat-page">

        <div class="mobile-section-header">
          <span class="mobile-section-title">
            ${this._inProject ? html`
              <button class="chat-page-back" title=${t('chat.mobile.back_general')}
                      @click=${() => this._exitProject()}>
                <i class="bi bi-chevron-left"></i>
              </button>
              <i class="bi bi-folder2-open"></i> ${this.label || t('chat.mobile.project')}
            ` : html`<i class="bi bi-chat-dots-fill"></i> ${t('chat.mobile.chat')}`}
          </span>
          <div class="chat-page-header-actions">
            <button
              class="btn btn-sm btn-outline-secondary"
              title=${t('chat.new_session')}
              @click=${() => this._startNewSession()}
            ><i class="bi bi-trash"></i></button>
          </div>
        </div>

        <div class="chat-page-messages">
          ${this._messages.length === 0 ? this._renderEmptyState() : this._messages.map(m => renderMsg(this, m))}

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

        <div class="chat-page-input-area">
          ${renderTaskStrip(this)}
          ${this._renderNoModelsBanner()}
          <div class="chat-page-composer"
               @dragover=${(e) => e.preventDefault()}
               @drop=${(e) => this._onDrop(e)}>
            ${renderAttachmentChips(this, this._attachments, { removable: true })}
            <input
              type="file"
              multiple
              class="chat-page-file-input"
              style="display:none"
              @change=${(e) => { this._addFiles(e.target.files); e.target.value = ''; }}
            />
            <textarea
              class="chat-page-textarea"
              rows="1"
              ?disabled=${this._noModels}
              placeholder=${t('chat.mobile.placeholder')}
              @input=${(e) => this._autoResize(e.target)}
              @paste=${(e) => this._onPaste(e)}
            ></textarea>
            <div class="chat-page-toolbar">
              <div class="chat-page-toolbar-left">
                <button
                  class="btn btn-sm btn-outline-secondary chat-page-attach-btn"
                  title=${t('chat.attach')}
                  @click=${() => this.querySelector('.chat-page-file-input')?.click()}
                ><i class="bi bi-paperclip"></i></button>
                ${this._providers.length > 1 ? html`
                  <select
                    class="chat-page-model-pill"
                    .value=${this._selectedClient ?? 'auto'}
                    @change=${(e) => { this._selectClient(e.target.value); }}
                  >
                    ${this._providers.map(p => html`
                      <option value=${p} ?selected=${p === (this._selectedClient ?? 'auto')}>${p}</option>
                    `)}
                  </select>
                ` : nothing}
              </div>
              <div class="chat-page-toolbar-right">
                ${this._hasTranscribe ? html`
                  <button
                    class="chat-page-mic-btn ${this._recording ? 'chat-page-mic-btn--recording' : ''}"
                    title=${this._recording ? t('chat.mobile.stop_record') : t('chat.mobile.record_voice')}
                    @click=${() => this._toggleRecording()}
                  >
                    <i class="bi ${this._recording ? 'bi-stop-circle-fill' : 'bi-mic-fill'}"></i>
                  </button>
                ` : nothing}
                ${this._waiting
                  ? html`<button
                      class="chat-page-send chat-page-send--stop"
                      @click=${() => this._cancel()}
                      title=${t('chat.stop')}
                    ><i class="bi bi-stop-fill"></i></button>`
                  : nothing}
                <button
                  class="chat-page-send"
                  ?disabled=${this._noModels}
                  @click=${() => this._send()}
                  title=${t('chat.send')}
                ><i class="bi bi-send-fill"></i></button>
              </div>
            </div>
          </div>
        </div>

      </div>
    `;
  }
}

customElements.define('chat-page', ChatPage);
