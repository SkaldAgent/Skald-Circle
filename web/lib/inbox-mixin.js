import { html, nothing } from 'lit';
import { InboxCardsMixin } from './inbox-cards.js';
import { t }               from './i18n.js';

/**
 * InboxMixin — the Inbox *page*: fetching every pending item of this user and
 * laying them out in sections.
 *
 * The cards themselves, and the calls that resolve them, live in
 * [`InboxCardsMixin`] — the chat renders the same ones for the pending items of
 * the background tasks it started.
 *
 * Used by AgentInboxPage (full page) and DashboardPage (embedded section).
 */
export const InboxMixin = (Base) => class extends InboxCardsMixin(Base) {

  static get properties() {
    return {
      ...super.properties,
      _inboxData:    { state: true },
      _inboxLoading: { state: true },
    };
  }

  constructor() {
    super();
    this._inboxData    = null;
    this._inboxLoading = false;
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

  /** Every resolved item changes this page's own list. */
  async _afterInboxResolve() {
    await this._loadInbox();
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
          <p>${t('inbox.empty')}</p>
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
