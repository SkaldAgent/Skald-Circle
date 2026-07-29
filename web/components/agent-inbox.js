import { html, nothing } from 'lit';
import { LightElement }  from '../lib/base.js';
import { InboxMixin }    from '../lib/inbox-mixin.js';
import { t, I18nMixin }  from '../lib/i18n.js';

export class AgentInboxPage extends I18nMixin(InboxMixin(LightElement)) {

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
    // Live refresh: the chat WS pushes `inbox-changed` when any of this user's
    // sessions raises or settles a pending item — reload immediately if open.
    this.__onInboxChanged = () => { if (this._open) this._loadInbox(); };
    window.addEventListener('inbox-changed', this.__onInboxChanged);
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
    window.removeEventListener('inbox-changed', this.__onInboxChanged);
  }

  _startPolling() {
    this._stopPolling();
    // Fallback only — pushes via `inbox-changed` keep the page fresh.
    this._pollTimer = setInterval(() => this._loadInbox(), 60000);
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
        <div class="page-header">
          <div class="page-header-left">
            <h2 class="page-header-title">
              ${t('nav.inbox')}
              ${total > 0 ? html`<span class="badge bg-danger ms-2">${total}</span>` : nothing}
            </h2>
          </div>
          <div class="page-header-actions">
            <button class="inbox-refresh-btn" title="Refresh" @click=${() => this._loadInbox()}>
              <i class="bi bi-arrow-clockwise"></i>
            </button>
          </div>
        </div>
        ${this._renderInboxSection()}
      </div>
    `;
  }
}
