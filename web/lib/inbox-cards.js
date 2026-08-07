import { html, nothing } from 'lit';
import { unsafeHTML }     from 'lit/directives/unsafe-html.js';
import { renderMarkdown } from './base.js';
import { t }              from './i18n.js';

/**
 * InboxCardsMixin — the pending-item cards and the calls that resolve them.
 *
 * Split out of `InboxMixin` when the chat started rendering the same cards for
 * the background tasks it started: an approval raised by an async sub-agent is
 * the *same* approval, resolved through the same endpoint, and a second
 * implementation of it would be two places to get a bypass scope wrong.
 *
 * The only thing the two surfaces disagree on is what to reload once an item is
 * resolved — the Inbox re-reads the whole inbox, the chat re-reads only its own
 * tasks' items. That is the `_afterInboxResolve` hook, and it is the whole seam.
 */
export const InboxCardsMixin = (Base) => class extends Base {

  static get properties() {
    return {
      ...super.properties,
      _inboxError: { state: true },
    };
  }

  constructor() {
    super();
    this._inboxError = null;
    // Raw-JSON disclosure per card, keyed `raw-{request_id}`. Deliberately not
    // named `_expanded`: the chat already has one of those for its tool cards.
    this._rawOpen    = new Set();
    this._bypassOpen = new Set();
  }

  /**
   * Called after an item is resolved, to refresh whatever list it came from.
   * Overridden by every consumer; a no-op default means a surface that forgets
   * shows a stale card rather than throwing.
   */
  async _afterInboxResolve() {}

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
      this._inboxError = null;
      await this._afterInboxResolve();
    } catch (e) {
      this._inboxError = e.message;
    }
  }

  _rejectWithNote(requestId, toolCallId = null) {
    const note = prompt(t('inbox.reject_prompt')) ?? '';
    this._resolveApproval(requestId, 'reject', note, null, null, toolCallId);
  }

  /**
   * Approve + skip approval for **this same tool** for a while.
   *
   * Scoped to the tool and nothing wider: the card the human just read is
   * about one call, and the previous category/MCP-server auto-detect meant a
   * click on "label this message" also un-gated "send this message" for the
   * rest of the session.
   */
  _approveWithBypass(item, bypassSecs) {
    this._resolveApproval(item.request_id, 'approve', '', bypassSecs, 'tool');
  }

  /** Short label for the bypass scope — the tool, `mcp__x__y` shown as `y`. */
  _bypassLabel(item) {
    const name = item.tool_name ?? '';
    return name.startsWith('mcp__') ? name.split('__').pop() : name;
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
      this._inboxError = null;
      await this._afterInboxResolve();
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
      this._inboxError = null;
      await this._afterInboxResolve();
    } catch (e) {
      this._inboxError = e.message;
    }
  }

  // ── Helpers ───────────────────────────────────────────────────────────────

  _toggleRaw(id) {
    if (this._rawOpen.has(id)) this._rawOpen.delete(id);
    else                       this._rawOpen.add(id);
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
    const open    = this._rawOpen.has(id);
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
            <i class="bi bi-check-lg"></i> ${t('approval.approve')}
          </button>
          <button class="btn btn-outline-danger"
                  @click=${() => this._rejectWithNote(item.request_id, item.tool_call_id)}>
            <i class="bi bi-x-lg"></i> ${t('approval.reject')}
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
                  title=${t('approval.bypass_all')}>
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
};
