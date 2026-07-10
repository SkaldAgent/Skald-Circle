import { html, nothing } from 'lit';
import { LightElement }  from '../lib/base.js';
import { InboxMixin }    from '../lib/inbox-mixin.js';

export class AgentInboxPage extends InboxMixin(LightElement) {

  static get properties() {
    return {
      ...super.properties,
      _open: { state: true },
    };
  }

  constructor() {
    super();
    this._open      = false;
    this._pollTimer = null;
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'inbox';
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) {
        this._loadInbox();
        this._startPolling();
      } else {
        this._stopPolling();
      }
    });
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    this._stopPolling();
  }

  _startPolling() {
    this._stopPolling();
    this._pollTimer = setInterval(() => this._loadInbox(), 8000);
  }

  _stopPolling() {
    if (this._pollTimer) {
      clearInterval(this._pollTimer);
      this._pollTimer = null;
    }
  }

  render() {
    const approvals      = this._inboxData?.approvals      ?? [];
    const clarifications = this._inboxData?.clarifications ?? [];
    const elicitations   = this._inboxData?.elicitations   ?? [];
    const total          = approvals.length + clarifications.length + elicitations.length;

    return html`
      <div class="page-panel">
        <div class="page-panel-header">
          <h5 class="mb-0">
            Agent Inbox
            ${total > 0 ? html`<span class="badge bg-danger ms-2">${total}</span>` : nothing}
          </h5>
          <button class="inbox-refresh-btn" title="Refresh" @click=${() => this._loadInbox()}>
            <i class="bi bi-arrow-clockwise"></i>
          </button>
        </div>
        ${this._renderInboxSection()}
      </div>
    `;
  }
}
