import { html, nothing } from 'lit';
import { unsafeHTML }       from 'lit/directives/unsafe-html.js';
import { renderMarkdown }   from './base.js';

/**
 * InboxMixin — shared fetch, action, and render logic for the agent inbox.
 * Used by AgentInboxPage (full page) and HomePage (embedded section).
 */
export const InboxMixin = (Base) => class extends Base {

  static get properties() {
    return {
      ...super.properties,
      _inboxData:    { state: true },
      _inboxError:   { state: true },
      _inboxLoading: { state: true },
    };
  }

  constructor() {
    super();
    this._inboxData    = null;
    this._inboxError   = null;
    this._inboxLoading = false;
    this._expanded     = new Set();
    this._bypassOpen   = new Set();
  }

  // ── Data ──────────────────────────────────────────────────────────────────

  async _loadInbox() {
    try {
      const res = await fetch('/api/inbox');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      this._inboxData  = await res.json();
      this._inboxError = null;
      window.dispatchEvent(new CustomEvent('inbox-count', { detail: { count: this._inboxData.total } }));
    } catch (e) {
      this._inboxError = e.message;
    }
  }

  // ── Actions ───────────────────────────────────────────────────────────────

  async _resolveApproval(requestId, action, note = '', bypassSecs = null, bypassScope = null, toolCallId = null) {
    try {
      const body = { action, note };
      if (bypassSecs !== null) {
        body.bypass_secs  = bypassSecs;
        body.bypass_scope = bypassScope;
      }
      // Live items resolve by request_id (and support bypass); DB-persisted
      // (post-restart) items carry request_id 0 → resolve by the durable,
      // source-agnostic tool_call_id (bypass buttons are hidden for them).
      const url = requestId
        ? `/api/inbox/approvals/${requestId}/resolve`
        : `/api/tools/${toolCallId}/resolve`;
      const res = await fetch(url, {
        method:  'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify(body),
      });
      if (!res.ok) throw new Error(await res.text());
      await this._loadInbox();
    } catch (e) {
      this._inboxError = e.message;
    }
  }

  _rejectWithNote(requestId, toolCallId = null) {
    const note = prompt('Rejection reason (optional):') ?? '';
    this._resolveApproval(requestId, 'reject', note, null, null, toolCallId);
  }

  /** Approve + set a timed or session bypass scoped to the tool's category or MCP server. */
  _approveWithBypass(item, bypassSecs) {
    const scope = item.tool_category ? 'category'
                : item.mcp_server    ? 'mcp_server'
                :                      'all';
    this._resolveApproval(item.request_id, 'approve', '', bypassSecs, scope);
  }

  /** Human-readable bypass scope label, e.g. "filesystem" or "Gmail". */
  _bypassLabel(item) {
    if (item.tool_category) return item.tool_category;
    if (item.mcp_server)    return item.mcp_server;
    return 'session';
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
      await this._loadInbox();
    } catch (e) {
      this._inboxError = e.message;
    }
  }

  /**
   * Resolve a server-initiated MCP elicitation. On `accept` with a field, the
   * input value is packed into `content` ({ [field]: value }); the secret is
   * sent once and never echoed back into the UI. `decline`/`cancel` send no value.
   */
  async _resolveElicitation(item, action, inputEl) {
    let content = null;
    if (action === 'accept' && item.field_name) {
      content = { [item.field_name]: inputEl ? inputEl.value : '' };
    }
    try {
      const res = await fetch(`/api/inbox/elicitations/${item.request_id}/resolve`, {
        method:  'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({ action, content }),
      });
      if (!res.ok) throw new Error(await res.text());
      await this._loadInbox();
    } catch (e) {
      this._inboxError = e.message;
    }
  }

  // ── Helpers ───────────────────────────────────────────────────────────────

  _toggleRaw(id) {
    if (this._expanded.has(id)) this._expanded.delete(id);
    else                        this._expanded.add(id);
    this.requestUpdate();
  }

  _toggleBypassMenu(id) {
    if (this._bypassOpen.has(id)) this._bypassOpen.delete(id);
    else                          this._bypassOpen.add(id);
    this.requestUpdate();
  }

  _fmt(iso) {
    if (!iso) return '';
    return new Date(iso).toLocaleString(undefined, {
      day: '2-digit', month: '2-digit', year: '2-digit',
      hour: '2-digit', minute: '2-digit',
    });
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

  // ── Card renderers ────────────────────────────────────────────────────────

  _renderApprovalCard(item) {
    const id      = `raw-${item.request_id}`;
    const open    = this._expanded.has(id);
    const label   = item.context_label ?? item.source;
    const args    = item.arguments ?? {};
    const keyArgs = this._keyArgs(args);
    const rawJson = JSON.stringify(args, null, 2);

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
          <pre class="inbox-args-raw ${open ? 'open' : ''}">${rawJson}</pre>
        </div>

        <div class="inbox-card-footer approval-footer">
          <button class="btn btn-success"
                  @click=${() => this._resolveApproval(item.request_id, 'approve', '', null, null, item.tool_call_id)}>
            <i class="bi bi-check-lg"></i> Approve
          </button>
          <button class="btn btn-outline-danger"
                  @click=${() => this._rejectWithNote(item.request_id, item.tool_call_id)}>
            <i class="bi bi-x-lg"></i> Reject
          </button>

          ${item.request_id ? html`
          <div class="inbox-bypass-wrap">
            <button class="btn btn-outline-secondary"
                    @click=${() => this._toggleBypassMenu(id)}>
              <i class="bi bi-clock-history"></i> ×${this._bypassLabel(item)} ▾
            </button>
            <div class="inbox-bypass-menu ${this._bypassOpen.has(id) ? 'open' : ''}">
              <button @click=${() => { this._bypassOpen.delete(id); this._approveWithBypass(item, 15 * 60); }}>
                15 min
              </button>
              <button @click=${() => { this._bypassOpen.delete(id); this._approveWithBypass(item, 60 * 60); }}>
                1 ora
              </button>
            </div>
          </div>

          <button class="btn btn-outline-secondary"
                  @click=${() => this._approveWithBypass(item, 0)}
                  title="Approva e non chiedere più per questa sessione">
            <i class="bi bi-shield-check"></i> Sessione
          </button>
          ` : nothing}
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
          <div class="inbox-question copilot-markdown">${unsafeHTML(renderMarkdown(item.question))}</div>

          ${item.suggested_answers?.length ? html`
            <div class="inbox-chips">
              ${item.suggested_answers.map(a => html`
                <button class="inbox-chip"
                        @click=${(e) => {
                          const inp = e.target.closest('.inbox-card')?.querySelector('.inbox-answer-input');
                          if (inp) { inp.value = a; inp.focus(); }
                        }}>
                  ${a}
                </button>
              `)}
            </div>
          ` : nothing}

          <div class="inbox-answer-area">
            <textarea class="inbox-answer-input" rows="2"
              placeholder="Your answer…"
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

  _renderElicitationCard(item) {
    const masked  = item.sensitive;
    const confirm = item.is_confirmation;

    return html`
      <div class="inbox-card elicitation-card">
        <div class="inbox-card-header">
          <span class="badge bg-secondary">
            <i class="bi ${masked ? 'bi-shield-lock' : 'bi-question-circle'}"></i>
            ${confirm ? 'Conferma' : 'Input'}
          </span>
          <span class="inbox-card-origin" title="${item.server_name}">${item.server_name}</span>
          <span class="inbox-card-time">${this._fmt(item.created_at)}</span>
        </div>

        <div class="inbox-card-body">
          <div class="inbox-question">${item.message}</div>

          ${confirm ? nothing : html`
            <div class="inbox-answer-area">
              <input class="inbox-answer-input inbox-secret-input"
                type="${masked ? 'password' : 'text'}"
                autocomplete="off" autocapitalize="off" autocorrect="off" spellcheck="false"
                placeholder="${masked ? '••••••••' : 'Value…'}"
                @keydown=${(e) => {
                  if (e.key === 'Enter') {
                    e.preventDefault();
                    this._resolveElicitation(item, 'accept', e.target);
                  }
                }}>
            </div>
          `}
        </div>

        <div class="inbox-card-footer approval-footer">
          <button class="btn btn-success"
                  @click=${(e) => {
                    const inp = e.target.closest('.inbox-card')?.querySelector('.inbox-secret-input');
                    this._resolveElicitation(item, 'accept', inp);
                  }}>
            <i class="bi bi-check-lg"></i> ${confirm ? 'Conferma' : 'Invia'}
          </button>
          <button class="btn btn-outline-danger"
                  @click=${() => this._resolveElicitation(item, 'decline', null)}>
            <i class="bi bi-x-lg"></i> Rifiuta
          </button>
        </div>
      </div>
    `;
  }

  // ── Section renderer (used by both full page and home embed) ─────────────

  _renderInboxSection() {
    const approvals      = this._inboxData?.approvals      ?? [];
    const clarifications = this._inboxData?.clarifications ?? [];
    const elicitations   = this._inboxData?.elicitations   ?? [];
    const total          = approvals.length + clarifications.length + elicitations.length;

    return html`
      ${this._inboxError ? html`
        <div class="alert alert-danger mx-3 mt-3">${this._inboxError}</div>
      ` : nothing}

      ${total === 0 ? html`
        <div class="inbox-empty">
          <i class="bi bi-inbox"></i>
          <p>No pending requests</p>
        </div>
      ` : html`
        <div class="inbox-grid">
          ${approvals.length > 0 ? html`
            <div class="inbox-section-header">
              <h6>Approvals</h6>
              <span class="badge bg-warning text-dark">${approvals.length}</span>
              <span class="section-line"></span>
            </div>
            ${approvals.map(item => this._renderApprovalCard(item))}
          ` : nothing}

          ${clarifications.length > 0 ? html`
            <div class="inbox-section-header">
              <h6>Questions</h6>
              <span class="badge bg-info text-dark">${clarifications.length}</span>
              <span class="section-line"></span>
            </div>
            ${clarifications.map(item => this._renderClarificationCard(item))}
          ` : nothing}

          ${elicitations.length > 0 ? html`
            <div class="inbox-section-header">
              <h6>Secrets</h6>
              <span class="badge bg-secondary">${elicitations.length}</span>
              <span class="section-line"></span>
            </div>
            ${elicitations.map(item => this._renderElicitationCard(item))}
          ` : nothing}
        </div>
      `}
    `;
  }
};
