import { html, nothing } from 'lit';
import { LightElement }  from '../../lib/base.js';

export class InboxPage extends LightElement {
  static properties = {
    visible:  { type: Boolean },
    _data:    { state: true },
    _error:   { state: true },
  };

  constructor() {
    super();
    this.visible   = false;
    this._data     = null;
    this._error    = null;
    this._pollTimer = null;
    this._expanded  = new Set();
  }

  connectedCallback() {
    super.connectedCallback();
    // Live refresh: the always-mounted <chat-page> keeps the chat WS connected
    // regardless of the active tab, and it re-dispatches inbox lifecycle events
    // from any of this user's sessions as the `inbox-changed` window event.
    // Reload immediately when visible instead of waiting for the next poll.
    this.__onInboxChanged = () => { if (this.visible) this._load(); };
    window.addEventListener('inbox-changed', this.__onInboxChanged);
  }

  updated(changed) {
    if (!changed.has('visible')) return;
    if (this.visible) {
      this._load();
      this._startPolling();
    } else {
      this._stopPolling();
    }
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    this._stopPolling();
    window.removeEventListener('inbox-changed', this.__onInboxChanged);
  }

  _startPolling() {
    this._stopPolling();
    // Fallback only — pushes via `inbox-changed` keep the page fresh.
    this._pollTimer = setInterval(() => this._load(), 60000);
  }

  _stopPolling() {
    if (this._pollTimer) { clearInterval(this._pollTimer); this._pollTimer = null; }
  }

  async _load() {
    try {
      const res = await fetch('/api/inbox');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      this._data  = await res.json();
      this._error = null;
    } catch (e) {
      this._error = e.message;
    }
  }

  async _resolveApproval(requestId, action, note = '', toolCallId = null) {
    try {
      // Live items resolve by request_id; DB-persisted (post-restart) items carry
      // request_id 0 → resolve by the durable, source-agnostic tool_call_id.
      const url = requestId
        ? `/api/inbox/approvals/${requestId}/resolve`
        : `/api/tools/${toolCallId}/resolve`;
      const res = await fetch(url, {
        method:  'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({ action, note }),
      });
      if (!res.ok) throw new Error(await res.text());
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  _rejectWithNote(requestId, toolCallId) {
    const note = prompt('Rejection reason (optional):') ?? '';
    this._resolveApproval(requestId, 'reject', note, toolCallId);
  }

  async _resolveClarification(requestId, inputEl) {
    const answer = inputEl.value.trim();
    if (!answer) return;
    try {
      const res = await fetch(`/api/inbox/clarifications/${requestId}/resolve`, {
        method:  'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({ answer }),
      });
      if (!res.ok) throw new Error(await res.text());
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  _toggleRaw(id) {
    if (this._expanded.has(id)) this._expanded.delete(id);
    else                        this._expanded.add(id);
    this.requestUpdate();
  }

  _keyArgs(args) {
    const entries = [];
    for (const key of ['path', 'command', 'url', 'origin', 'destination', 'name', 'message', 'query']) {
      if (args[key] !== undefined) {
        let val = args[key];
        if (typeof val === 'object') val = JSON.stringify(val);
        entries.push({ key, value: String(val) });
      }
    }
    return entries;
  }

  _fmt(iso) {
    if (!iso) return '';
    return new Date(iso).toLocaleString(undefined, {
      day: '2-digit', month: '2-digit', year: '2-digit',
      hour: '2-digit', minute: '2-digit',
    });
  }

  _renderApprovalCard(item) {
    const id      = `raw-${item.request_id}`;
    const open    = this._expanded.has(id);
    const label   = item.context_label ?? item.source;
    const args    = item.arguments ?? {};
    const keyArgs = this._keyArgs(args);

    return html`
      <div class="inbox-card approval-card">
        <div class="inbox-card-header">
          <span class="badge bg-warning text-dark">Approval</span>
          <span class="inbox-card-origin" title="${label}">${label}</span>
          <span class="inbox-card-time">${this._fmt(item.created_at)}</span>
        </div>

        <div class="inbox-card-body">
          <div class="inbox-tool-name">
            <i class="bi bi-tools"></i>
            <strong>${item.tool_name}</strong>
            <span class="inbox-agent-tag">
              <i class="bi bi-person"></i> ${item.agent_id}
            </span>
          </div>

          ${keyArgs.length > 0 ? html`
            <div class="inbox-args-structured">
              ${keyArgs.map(kv => html`
                <div class="inbox-arg-row">
                  <span class="inbox-arg-key">${kv.key}</span>
                  <span class="inbox-arg-value">${kv.value}</span>
                </div>
              `)}
            </div>
          ` : nothing}

          <button class="inbox-args-toggle" @click=${() => this._toggleRaw(id)}>
            <i class="bi ${open ? 'bi-chevron-up' : 'bi-chevron-down'}"></i>
            ${open ? 'Hide raw JSON' : 'Show raw JSON'}
          </button>
          <pre class="inbox-args-raw ${open ? 'open' : ''}">${JSON.stringify(args, null, 2)}</pre>
        </div>

        <div class="inbox-card-footer">
          <button class="inbox-btn inbox-btn-approve"
                  @click=${() => this._resolveApproval(item.request_id, 'approve', '', item.tool_call_id)}>
            <i class="bi bi-check-lg"></i> Approve
          </button>
          <button class="inbox-btn inbox-btn-reject"
                  @click=${() => this._rejectWithNote(item.request_id, item.tool_call_id)}>
            <i class="bi bi-x-lg"></i> Reject
          </button>
        </div>
      </div>
    `;
  }

  _renderClarificationCard(item) {
    const label = item.context_label ?? item.source;

    return html`
      <div class="inbox-card clarification-card">
        <div class="inbox-card-header">
          <span class="badge bg-info text-dark">Question</span>
          <span class="inbox-card-origin" title="${label}">${label}</span>
          <span class="inbox-card-time">${this._fmt(item.created_at)}</span>
        </div>

        <div class="inbox-card-body">
          <div class="inbox-card-title">${item.title}</div>
          <div class="inbox-question">${item.question}</div>

          ${item.suggested_answers?.length ? html`
            <div class="inbox-chips">
              ${item.suggested_answers.map(a => html`
                <button class="inbox-chip"
                        @click=${(e) => {
                          const inp = e.target.closest('.inbox-card')?.querySelector('.inbox-answer-input');
                          if (inp) { inp.value = a; inp.focus(); }
                        }}>${a}</button>
              `)}
            </div>
          ` : nothing}

          <div class="inbox-answer-area">
            <textarea class="inbox-answer-input" rows="2" placeholder="Your answer…"
              @keydown=${(e) => {
                if (e.key === 'Enter' && !e.shiftKey) {
                  e.preventDefault();
                  this._resolveClarification(item.request_id, e.target);
                }
              }}></textarea>
            <button class="inbox-answer-send"
                    @click=${(e) => {
                      const inp = e.target.closest('.inbox-card')?.querySelector('.inbox-answer-input');
                      if (inp) this._resolveClarification(item.request_id, inp);
                    }}>
              <i class="bi bi-send"></i> Send
            </button>
          </div>
        </div>
      </div>
    `;
  }

  render() {
    if (!this.visible) return nothing;

    const approvals      = this._data?.approvals      ?? [];
    const clarifications = this._data?.clarifications ?? [];
    const total          = approvals.length + clarifications.length;

    return html`
      <div class="mobile-inbox">
        <div class="mobile-section-header">
          <span class="mobile-section-title">
            Inbox
            ${total > 0 ? html`<span class="badge bg-danger ms-2">${total}</span>` : nothing}
          </span>
          <button class="inbox-refresh-btn" @click=${() => this._load()}>
            <i class="bi bi-arrow-clockwise"></i>
          </button>
        </div>

        ${this._error ? html`
          <div class="mobile-alert-error">${this._error}</div>
        ` : nothing}

        ${total === 0 ? html`
          <div class="inbox-empty">
            <i class="bi bi-inbox"></i>
            <p>No pending requests</p>
          </div>
        ` : html`
          <div class="mobile-inbox-list">
            ${approvals.length > 0 ? html`
              <div class="inbox-section-label">
                Approvals <span class="badge bg-warning text-dark">${approvals.length}</span>
              </div>
              ${approvals.map(item => this._renderApprovalCard(item))}
            ` : nothing}

            ${clarifications.length > 0 ? html`
              <div class="inbox-section-label">
                Questions <span class="badge bg-info text-dark">${clarifications.length}</span>
              </div>
              ${clarifications.map(item => this._renderClarificationCard(item))}
            ` : nothing}
          </div>
        `}
      </div>
    `;
  }
}

customElements.define('inbox-page', InboxPage);
