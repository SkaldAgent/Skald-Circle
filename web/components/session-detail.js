import { html, nothing } from 'lit';
import { unsafeHTML }     from 'lit/directives/unsafe-html.js';
import { LightElement } from '../lib/base.js';
import { t }            from '../lib/i18n.js';

const PAGE_ID = 'session';

function formatDate(iso) {
  if (!iso) return '—';
  return new Date(iso).toLocaleString(undefined, {
    day: '2-digit', month: '2-digit', year: '2-digit',
    hour: '2-digit', minute: '2-digit', second: '2-digit',
  });
}

function formatTime(iso) {
  if (!iso) return null;
  return new Date(iso).toLocaleTimeString('en-GB', {
    hour: '2-digit', minute: '2-digit', second: '2-digit',
  });
}

function sourceBadgeClass(source) {
  const map = { tic: 'bg-warning text-dark', cron: 'bg-info text-dark', web: 'bg-primary', telegram: 'bg-success', mobile: 'bg-secondary' };
  return map[source] ?? 'bg-secondary';
}

function jsonPretty(val) {
  if (val == null) return '—';
  if (typeof val === 'string') return val;
  return JSON.stringify(val, null, 2);
}

export class SessionDetailPage extends LightElement {
  static properties = {
    _open:            { state: true },
    _sessionId:       { state: true },
    _data:            { state: true },
    _loading:         { state: true },
    _error:           { state: true },
    _live:            { state: true },
    _expandedTools:   { state: true },
    _expandedReasons: { state: true },
  };

  constructor() {
    super();
    this._open            = false;
    this._sessionId       = null;
    this._data            = null;
    this._loading         = false;
    this._error           = null;
    this._live            = false;
    this._expandedTools   = new Set();
    this._expandedReasons = new Set();
    this._ws              = null;
    this._wsReconnectTimer = null;
  }

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === PAGE_ID;
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) this._loadFromHash();
      else            this._closeWs();
    });
    window.addEventListener('hashchange', () => {
      if (this._open) this._loadFromHash();
    });
  }

  disconnectedCallback() {
    window.removeEventListener('locale-changed', this.__onLocaleChanged);
    super.disconnectedCallback();
    this._closeWs();
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    this._closeWs();
  }

  _idFromHash() {
    const parts = location.hash.replace('#', '').split('/');
    if (parts[0] === PAGE_ID && parts[1]) return parseInt(parts[1], 10);
    return null;
  }

  _loadFromHash() {
    const id = this._idFromHash();
    if (id != null && id !== this._sessionId) {
      this._sessionId = id;
      this._closeWs();
      this._fetch(id);
    }
  }

  async _fetch(id) {
    this._loading = true;
    this._error   = null;
    this._data    = null;
    this._expandedTools   = new Set();
    this._expandedReasons = new Set();
    try {
      const res = await fetch(`/api/sessions/${id}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      this._data = await res.json();
      this._connectWs(id);
    } catch (e) {
      this._error = e.message;
    } finally {
      this._loading = false;
    }
  }

  // ── Live WebSocket ─────────────────────────────────────────────────────────

  _connectWs(id) {
    this._closeWs();
    const proto = location.protocol === 'https:' ? 'wss' : 'ws';
    const ws = new WebSocket(`${proto}://${location.host}/api/ws/session/${id}`);
    this._ws = ws;

    ws.onopen = () => { this._live = true; };

    ws.onmessage = (e) => {
      try { this._handleEvent(JSON.parse(e.data)); } catch {}
    };

    ws.onclose = () => {
      this._live = false;
      this._ws = null;
      // Reconnect after 3 s if the page is still open and showing this session.
      if (this._open && this._sessionId === id) {
        this._wsReconnectTimer = setTimeout(() => this._connectWs(id), 3000);
      }
    };

    ws.onerror = () => ws.close();
  }

  _closeWs() {
    clearTimeout(this._wsReconnectTimer);
    if (this._ws) { this._ws.onclose = null; this._ws.close(); this._ws = null; }
    this._live = false;
  }

  _handleEvent(ev) {
    if (!this._data) return;
    const msgs = [...this._data.messages];
    const now  = new Date().toISOString();

    switch (ev.type) {
      case 'tool_start':
        msgs.push({
          kind:         'tool',
          tool_call_id: ev.tool_call_id,
          message_id:   ev.message_id,
          name:         ev.name,
          label_short:  ev.label_short,
          label_full:   ev.label_full,
          arguments:    ev.arguments,
          result:       null,
          error:        null,
          status:       'pending',
          created_at:   now,
        });
        break;

      case 'tool_done': {
        const i = msgs.findIndex(m => m.kind === 'tool' && m.tool_call_id === ev.tool_call_id);
        if (i >= 0) msgs[i] = { ...msgs[i], result: ev.result, status: 'done' };
        break;
      }

      case 'tool_error': {
        const i = msgs.findIndex(m => m.kind === 'tool' && m.tool_call_id === ev.tool_call_id);
        if (i >= 0) msgs[i] = { ...msgs[i], error: ev.error, status: 'error' };
        break;
      }

      case 'thinking':
        msgs.push({
          kind:         'thinking',
          message_id:   ev.message_id,
          content:      ev.content,
          reasoning:    '',
          input_tokens:  ev.input_tokens  ?? null,
          output_tokens: ev.output_tokens ?? null,
          created_at:   now,
        });
        break;

      case 'done':
        msgs.push({
          kind:         'assistant',
          message_id:   ev.message_id,
          content:      ev.content,
          reasoning:    '',
          input_tokens:  ev.input_tokens  ?? null,
          output_tokens: ev.output_tokens ?? null,
          created_at:   now,
        });
        break;

      case 'user_message':
        msgs.push({
          kind:         'user',
          content:      ev.content,
          is_synthetic: false,
          created_at:   now,
        });
        break;

      case 'agent_start':
        msgs.push({
          kind:     'agent',
          agent_id: ev.agent_id,
          depth:    ev.depth,
        });
        break;

      case 'agent_done':
        msgs.push({
          kind:     'agent_end',
          agent_id: ev.agent_id,
        });
        break;

      default:
        return; // ignore unknown events
    }

    this._data = { ...this._data, messages: msgs };
  }

  _toggleTool(id) {
    const next = new Set(this._expandedTools);
    next.has(id) ? next.delete(id) : next.add(id);
    this._expandedTools = next;
  }

  _toggleReason(id) {
    const next = new Set(this._expandedReasons);
    next.has(id) ? next.delete(id) : next.add(id);
    this._expandedReasons = next;
  }

  // ── Renderers ─────────────────────────────────────────────────────────────────

  _back() {
    history.back();
  }

  _renderSessionHeader(session) {
    return html`
      <div class="sd-session-header">
        <div class="d-flex align-items-center gap-2 flex-wrap">
          <button class="btn btn-sm btn-outline-secondary sd-back-btn" @click=${() => this._back()}>
            <i class="bi bi-arrow-left"></i> ${t('session.back')}
          </button>
          <span class="badge ${sourceBadgeClass(session.source)}">${session.source}</span>
          <span class="fw-semibold font-monospace">${t('session.agent')} ${session.agent_id}</span>
          <span class="text-secondary small">${t('session.id')} ${session.id}</span>
          ${session.is_ephemeral ? html`<span class="badge bg-light text-dark border">${t('session.ephemeral')}</span>` : nothing}
          ${!session.is_interactive ? html`<span class="badge bg-light text-dark border">${t('session.automated')}</span>` : nothing}
          ${this._live
            ? html`<span class="sd-live-badge"><span class="sd-live-dot"></span>${t('session.live')}</span>`
            : nothing}
        </div>
        <div class="text-secondary small mt-1">${formatDate(session.created_at)}</div>
      </div>
    `;
  }

  _renderUserMsg(item, idx) {
    const time = formatTime(item.created_at);
    return html`
      <div class="sd-msg sd-msg--user ${item.is_synthetic ? 'sd-msg--synthetic' : ''}">
        <div class="sd-msg-role">
          ${item.is_synthetic
            ? html`<span class="badge bg-warning text-dark me-1" style="font-size:0.65rem">${t('session.synthetic')}</span>`
            : nothing}
          <span>${t('session.user_role')}</span>
          ${time ? html`<span class="sd-msg-time">${time}</span>` : nothing}
        </div>
        <div class="sd-msg-content">${item.content}</div>
        ${item.failed ? html`<div class="sd-msg-failed">${t('session.failed')}</div>` : nothing}
      </div>
    `;
  }

  _renderAssistantMsg(item, idx) {
    const key = `ast-${idx}`;
    const hasReasoning = item.reasoning && item.reasoning.trim().length > 0;
    const expanded = this._expandedReasons.has(key);
    const time = formatTime(item.created_at);
    return html`
      <div class="sd-msg sd-msg--assistant ${item.failed ? 'sd-msg--failed' : ''}">
        <div class="sd-msg-role">
          ${t('session.assistant_role')}
          ${time ? html`<span class="sd-msg-time">${time}</span>` : nothing}
          ${item.input_tokens != null ? html`<span class="sd-tokens">${item.input_tokens}↑ ${item.output_tokens}↓</span>` : nothing}
        </div>
        ${hasReasoning ? html`
          <div class="sd-reasoning-toggle" @click=${() => this._toggleReason(key)}>
            <i class="bi bi-brain me-1"></i>
            ${t('session.reasoning_label')}
            <i class="bi bi-chevron-${expanded ? 'up' : 'down'} ms-1"></i>
          </div>
          ${expanded ? html`<pre class="sd-reasoning-block">${item.reasoning}</pre>` : nothing}
        ` : nothing}
        ${item.content ? html`<div class="sd-msg-content">${item.content}</div>` : nothing}
      </div>
    `;
  }

  _renderThinkingMsg(item, idx) {
    const key = `think-${item.message_id ?? idx}`;
    const hasReasoning = item.reasoning && item.reasoning.trim().length > 0;
    const expanded = this._expandedReasons.has(key);
    const time = formatTime(item.created_at);
    return html`
      <div class="sd-msg sd-msg--thinking ${item.failed ? 'sd-msg--failed' : ''}">
        <div class="sd-msg-role">
          <i class="bi bi-lightning-charge me-1"></i>${t('session.thinking_role')}
          ${time ? html`<span class="sd-msg-time">${time}</span>` : nothing}
          ${item.input_tokens != null ? html`<span class="sd-tokens">${item.input_tokens}↑ ${item.output_tokens}↓</span>` : nothing}
        </div>
        ${hasReasoning ? html`
          <div class="sd-reasoning-toggle" @click=${() => this._toggleReason(key)}>
            <i class="bi bi-brain me-1"></i>
            ${t('session.reasoning_label')}
            <i class="bi bi-chevron-${expanded ? 'up' : 'down'} ms-1"></i>
          </div>
          ${expanded ? html`<pre class="sd-reasoning-block">${item.reasoning}</pre>` : nothing}
        ` : nothing}
        ${item.content ? html`<div class="sd-msg-content">${item.content}</div>` : nothing}
      </div>
    `;
  }

  _renderToolMsg(item, idx) {
    const key = item.tool_call_id ?? idx;
    const expanded = this._expandedTools.has(key);
    const statusClass = { done: 'sd-tool--done', error: 'sd-tool--error', pending: 'sd-tool--pending' }[item.status] ?? '';
    return html`
      <div class="sd-tool ${statusClass}">
        <div class="sd-tool-header" @click=${() => this._toggleTool(key)}>
          <span class="sd-tool-icon">
            <i class="bi bi-${item.status === 'done' ? 'check-circle' : item.status === 'error' ? 'x-circle' : 'hourglass-split'}"></i>
          </span>
          <span class="sd-tool-name">${item.label_short ?? item.name}</span>
          <i class="bi bi-chevron-${expanded ? 'up' : 'down'} ms-auto"></i>
        </div>
        ${expanded ? html`
          <div class="sd-tool-body">
            ${item.label_full && item.label_full !== item.label_short
              ? html`<div class="sd-tool-label-full text-secondary small mb-2">${item.label_full}</div>`
              : nothing}
            <div class="sd-tool-section-label">${t('session.tool_args')}</div>
            <pre class="sd-code-block">${jsonPretty(item.arguments)}</pre>
            <div class="sd-tool-section-label mt-2">
              ${item.status === 'error' ? t('session.tool_error') : t('session.tool_result')}
            </div>
            <pre class="sd-code-block ${item.status === 'error' ? 'sd-code-block--error' : ''}">${
              item.result ?? item.error ?? '—'
            }</pre>
          </div>
        ` : nothing}
      </div>
    `;
  }

  _renderAgentFrame(item) {
    return html`
      <div class="sd-agent-frame-start">
        <i class="bi bi-robot me-1"></i>
        <span>${t('session.sub_agent')} <strong>${item.agent_id}</strong></span>
        <span class="text-secondary small ms-2">${t('session.depth', { n: item.depth })}</span>
      </div>
    `;
  }

  _renderAgentFrameEnd(item) {
    return html`<div class="sd-agent-frame-end">${t('session.end_of')} ${item.agent_id}</div>`;
  }

  _renderMessage(item, idx) {
    switch (item.kind) {
      case 'user':      return this._renderUserMsg(item, idx);
      case 'assistant': return this._renderAssistantMsg(item, idx);
      case 'thinking':  return this._renderThinkingMsg(item, idx);
      case 'tool':      return this._renderToolMsg(item, idx);
      case 'agent':     return this._renderAgentFrame(item);
      case 'agent_end': return this._renderAgentFrameEnd(item);
      default:          return nothing;
    }
  }

  render() {
    return html`
      <style>
        .sd-container {
          display: flex;
          flex-direction: column;
          flex: 1;
          min-height: 0;
          overflow-y: auto;
          padding: 1.5rem;
          gap: 0;
        }
        .sd-back-btn {
          font-size: 0.8rem;
          padding: 0.2rem 0.6rem;
        }
        .sd-session-header {
          background: var(--bs-tertiary-bg);
          border: 1px solid var(--bs-border-color);
          border-radius: 0.5rem;
          padding: 0.875rem 1rem;
          margin-bottom: 1.25rem;
        }
        .sd-msg {
          border-left: 3px solid transparent;
          padding: 0.625rem 0.875rem;
          margin-bottom: 0.5rem;
          border-radius: 0 0.375rem 0.375rem 0;
          background: var(--bs-body-bg);
        }
        .sd-msg--user        { border-left-color: var(--bs-primary); background: var(--bs-tertiary-bg); }
        .sd-msg--synthetic   { opacity: 0.75; border-left-color: var(--bs-warning); }
        .sd-msg--assistant   { border-left-color: var(--bs-success); }
        .sd-msg--thinking    { border-left-color: var(--bs-info); background: var(--bs-tertiary-bg); }
        .sd-msg--failed      { border-left-color: var(--bs-danger) !important; opacity: 0.7; }
        .sd-msg-role {
          font-size: 0.7rem;
          font-weight: 600;
          text-transform: uppercase;
          letter-spacing: 0.05em;
          color: var(--bs-secondary-color);
          margin-bottom: 0.25rem;
          display: flex;
          align-items: center;
          gap: 0.375rem;
        }
        .sd-msg-time {
          font-weight: 400;
          font-size: 0.65rem;
          color: var(--bs-secondary-color);
          font-family: var(--bs-font-monospace);
        }
        .sd-tokens {
          font-weight: 400;
          font-size: 0.65rem;
          color: var(--bs-secondary-color);
          margin-left: auto;
        }
        .sd-msg-content {
          white-space: pre-wrap;
          word-break: break-word;
          font-size: 0.875rem;
          line-height: 1.55;
        }
        .sd-msg-failed { font-size: 0.7rem; color: var(--bs-danger); margin-top: 0.25rem; }
        .sd-reasoning-toggle {
          display: inline-flex;
          align-items: center;
          cursor: pointer;
          font-size: 0.7rem;
          color: var(--bs-secondary-color);
          border: 1px solid var(--bs-border-color);
          border-radius: 999px;
          padding: 0.125rem 0.5rem;
          margin-bottom: 0.375rem;
          user-select: none;
        }
        .sd-reasoning-toggle:hover { background: var(--bs-tertiary-bg); }
        .sd-reasoning-block {
          background: var(--bs-tertiary-bg);
          border: 1px solid var(--bs-border-color);
          border-radius: 0.375rem;
          padding: 0.625rem 0.75rem;
          font-size: 0.78rem;
          white-space: pre-wrap;
          word-break: break-word;
          margin-bottom: 0.5rem;
          max-height: 20rem;
          overflow-y: auto;
          color: var(--bs-secondary-color);
        }
        .sd-tool {
          border: 1px solid var(--bs-border-color);
          border-radius: 0.375rem;
          margin-bottom: 0.375rem;
        }
        .sd-tool--done    { border-left: 3px solid var(--bs-success); }
        .sd-tool--error   { border-left: 3px solid var(--bs-danger); }
        .sd-tool--pending { border-left: 3px solid var(--bs-warning); }
        .sd-tool-header {
          display: flex;
          align-items: center;
          gap: 0.5rem;
          padding: 0.5rem 0.75rem;
          cursor: pointer;
          user-select: none;
          font-size: 0.82rem;
          font-weight: 500;
        }
        .sd-tool-header:hover { background: var(--bs-tertiary-bg); }
        .sd-tool-icon { color: var(--bs-secondary-color); }
        .sd-tool-name { font-family: var(--bs-font-monospace); font-size: 0.8rem; }
        .sd-tool-body {
          padding: 0.625rem 0.75rem;
          border-top: 1px solid var(--bs-border-color);
          background: var(--bs-body-bg);
        }
        .sd-tool-section-label {
          font-size: 0.68rem;
          font-weight: 600;
          text-transform: uppercase;
          letter-spacing: 0.05em;
          color: var(--bs-secondary-color);
          margin-bottom: 0.25rem;
        }
        .sd-tool-label-full { font-style: italic; }
        .sd-code-block {
          background: var(--bs-tertiary-bg);
          border: 1px solid var(--bs-border-color);
          border-radius: 0.25rem;
          padding: 0.5rem 0.625rem;
          font-size: 0.75rem;
          white-space: pre-wrap;
          word-break: break-word;
          max-height: 16rem;
          overflow-y: auto;
          margin: 0;
        }
        .sd-code-block--error { border-color: var(--bs-danger); color: var(--bs-danger); }
        .sd-agent-frame-start {
          display: flex;
          align-items: center;
          gap: 0.25rem;
          font-size: 0.78rem;
          color: var(--bs-secondary-color);
          border-top: 1px dashed var(--bs-border-color);
          padding: 0.375rem 0;
          margin: 0.25rem 0 0.25rem 1rem;
        }
        .sd-agent-frame-end {
          font-size: 0.72rem;
          color: var(--bs-secondary-color);
          border-bottom: 1px dashed var(--bs-border-color);
          padding: 0.25rem 0;
          margin: 0 0 0.375rem 1rem;
        }
        .sd-live-badge {
          display: inline-flex;
          align-items: center;
          gap: 0.3rem;
          font-size: 0.65rem;
          font-weight: 600;
          text-transform: uppercase;
          letter-spacing: 0.05em;
          color: var(--bs-success);
          border: 1px solid var(--bs-success);
          border-radius: 999px;
          padding: 0.1rem 0.45rem;
        }
        .sd-live-dot {
          width: 6px;
          height: 6px;
          border-radius: 50%;
          background: var(--bs-success);
          animation: sd-pulse 1.4s ease-in-out infinite;
        }
        @keyframes sd-pulse {
          0%, 100% { opacity: 1; }
          50%       { opacity: 0.25; }
        }
      </style>

      <div class="sd-container">
        ${this._loading ? html`
          <div class="text-center text-secondary py-5">
            <div class="spinner-border spinner-border-sm me-2"></div>${t('session.loading')}
          </div>
        ` : this._error ? html`
          <div class="alert alert-danger">${this._error}</div>
        ` : !this._data ? html`
          <div class="text-secondary text-center py-5">${t('session.no_session')}<br>
            <span class="small">${unsafeHTML(t('session.no_session_hint'))}</span>
          </div>
        ` : html`
          ${this._renderSessionHeader(this._data.session)}
          ${this._data.messages.length === 0
            ? html`<div class="text-secondary text-center py-4">${t('session.empty')}</div>`
            : this._data.messages.map((m, i) => this._renderMessage(m, i))
          }
        `}
      </div>
    `;
  }
}
