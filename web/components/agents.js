import { html }        from 'lit';
import { unsafeHTML }  from 'lit/directives/unsafe-html.js';
import { LightElement, renderMarkdown } from '../lib/base.js';
import { t }           from '../lib/i18n.js';

const STRENGTH_COLORS = {
  very_high: '#ef4444',
  high:      '#f97316',
  average:   '#eab308',
  low:       '#84cc16',
  very_low:  '#22c55e',
};

const STRENGTH_LABELS = {
  very_high: 'Very High',
  high:      'High',
  average:   'Average',
  low:       'Low',
  very_low:  'Very Low',
};

export class AgentsPage extends LightElement {
  static properties = {
    _open:     { state: true },
    _agents:   { state: true },
    _detail:   { state: true }, // null | { meta, prompt, models }
    _loading:  { state: true },
    _error:    { state: true },
  };

  constructor() {
    super();
    this._open    = false;
    this._agents  = [];
    this._detail  = null;
    this._loading = false;
    this._error   = null;
  }

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => {
      // The user-facing agent name/description are localized server-side, so a
      // language switch must refetch — a bare re-render would keep the strings
      // fetched under the previous locale.
      if (this._open) {
        if (this._detail) this._openDetail(this._detail.meta);
        else this._loadList();
      }
      this.requestUpdate();
    };
    window.addEventListener('locale-changed', this.__onLocaleChanged);
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'agents';
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open && this._agents.length === 0) this._loadList();
      if (!this._open) this._detail = null;
    });
  }

  disconnectedCallback() {
    window.removeEventListener('locale-changed', this.__onLocaleChanged);
    super.disconnectedCallback();
  }

  async _loadList() {
    this._loading = true;
    this._error   = null;
    try {
      const res = await fetch('/api/agents');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      this._agents = await res.json();
    } catch (e) {
      this._error = e.message;
    } finally {
      this._loading = false;
    }
  }

  async _openDetail(agent) {
    this._loading = true;
    this._error   = null;
    try {
      const res = await fetch(`/api/agents/${agent.id}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      this._detail = await res.json();
    } catch (e) {
      this._error = e.message;
    } finally {
      this._loading = false;
    }
  }

  _back() {
    this._detail = null;
    this._error  = null;
  }

  // ── Render helpers ────────────────────────────────────────────────────────

  _strengthLabel(strength) {
    return { very_high: t('agents.strength.very_high'), high: t('agents.strength.high'), average: t('agents.strength.average'), low: t('agents.strength.low'), very_low: t('agents.strength.very_low') }[strength] ?? strength;
  }

  _strengthDot(strength, size = '0.62rem') {
    if (!strength) return html`<span style="opacity:0.3;font-size:${size}">${'—'}</span>`;
    return html`
      <span class="agent-strength-dot"
            style="background:${STRENGTH_COLORS[strength] ?? '#888'}"
            title=${this._strengthLabel(strength)}></span>
    `;
  }

  // ── List view ─────────────────────────────────────────────────────────────

  _renderCard(agent) {
    return html`
      <div class="agent-card" @click=${() => this._openDetail(agent)}>
        <div class="agent-card-body">
          ${agent.icon ? html`
            <img class="agent-card-icon" src="/api/agents/${agent.id}/icon" alt="${agent.name}" loading="lazy">
          ` : ''}
          <div class="agent-card-content">
            <div class="agent-card-header">
              <span class="agent-card-name">${agent.name}</span>
              <span class="agent-card-id text-muted">${agent.id}</span>
            </div>
            <p class="agent-card-desc text-muted">${agent.friendly_description ?? agent.description}</p>
            <div class="agent-card-meta">
              ${agent.strength ? html`
                <span class="agent-meta-item">
                  ${this._strengthDot(agent.strength)}
                  <span>${this._strengthLabel(agent.strength)}</span>
                </span>
              ` : ''}
              ${agent.client ? html`
                <span class="agent-meta-item text-muted" style="font-size:0.75rem">
                  <i class="bi bi-pin-fill me-1" style="font-size:0.65rem"></i>${agent.client}
                </span>
              ` : ''}
            </div>
          </div>
        </div>
      </div>
    `;
  }

  _renderSection(title, agents) {
    if (agents.length === 0) return '';
    return html`
      <section class="agent-group">
        <h3 class="agent-group-title">${title}</h3>
        <div class="agent-grid">
          ${agents.map(a => this._renderCard(a))}
        </div>
      </section>
    `;
  }

  _renderList() {
    if (this._loading) return html`<div class="text-muted py-4 text-center">${t('agents.loading')}</div>`;
    if (this._error)   return html`<div class="alert alert-danger py-2" style="font-size:0.85rem">${this._error}</div>`;
    if (this._agents.length === 0) return html`<p class="text-muted">${t('agents.empty')}</p>`;
    const chat   = this._agents.filter(a => a.type === 'chat');
    const task   = this._agents.filter(a => a.type === 'task');
    const system = this._agents.filter(a => a.type === 'system');
    return html`
      ${this._renderSection(t('agents.section.chat'), chat)}
      ${this._renderSection(t('agents.section.task'), task)}
      ${this._renderSection(t('agents.section.system'), system)}
    `;
  }

  // ── Detail view ───────────────────────────────────────────────────────────

  _renderModelRow(m, i) {
    const isFirst = i === 0;
    return html`
      <tr class="${isFirst ? 'agent-model-row--first' : ''}">
        <td class="agent-model-rank text-muted">${i + 1}</td>
        <td>${this._strengthDot(m.strength)}</td>
        <td>
          <span class="fw-semibold">${m.name}</span>
          ${m.is_default ? html`<span class="badge bg-primary ms-1" style="font-size:0.6rem">${t('agents.detail.default')}</span>` : ''}
        </td>
        <td class="text-muted agent-model-id">${m.model_id}</td>
      </tr>
    `;
  }

  _renderDetail() {
    if (this._loading && !this._detail) return html`<div class="text-muted py-4 text-center">${t('agents.loading')}</div>`;
    if (!this._detail) return '';

    const { meta, prompt, models } = this._detail;

    return html`
      <div class="agent-detail">
        <div class="agent-detail-header">
          <button class="btn btn-sm btn-link px-0" @click=${() => this._back()}>
            <i class="bi bi-arrow-left me-1"></i>${t('agents.back')}
          </button>
          <div class="agent-detail-title-row">
            ${meta.icon ? html`
              <img class="agent-detail-icon" src="/api/agents/${meta.id}/icon" alt="${meta.name}">
            ` : ''}
            <div>
              <h2 class="agent-detail-title">${meta.name}</h2>
              <p class="text-muted mb-0" style="font-size:0.9rem">${meta.friendly_description ?? meta.description}</p>
            </div>
          </div>
        </div>

        ${this._error ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:0.85rem">${this._error}</div>` : ''}

        <div class="agent-detail-body">
          <section class="agent-section">
            <h3 class="agent-section-title">${t('agents.detail.meta')}</h3>
            <table class="agent-meta-table">
              <tbody>
                <tr><td class="agent-meta-key">${t('agents.detail.id')}</td><td><code>${meta.id}</code></td></tr>
                ${meta.strength ? html`
                  <tr><td class="agent-meta-key">${t('agents.detail.strength')}</td>
                    <td class="d-flex align-items-center gap-2">
                      ${this._strengthDot(meta.strength)}
                      ${this._strengthLabel(meta.strength)}
                    </td>
                  </tr>
                ` : ''}
                ${meta.client ? html`
                  <tr><td class="agent-meta-key">${t('agents.detail.pinned_model')}</td><td><code>${meta.client}</code></td></tr>
                ` : ''}
                ${meta.inject_memory?.length ? html`
                  <tr><td class="agent-meta-key">${t('agents.detail.memory_files')}</td>
                    <td>${meta.inject_memory.map(f => html`<div style="font-size:0.8rem"><code>${f}</code></div>`)}</td>
                  </tr>
                ` : ''}
              </tbody>
            </table>
          </section>

          <section class="agent-section">
            <h3 class="agent-section-title">${t('agents.detail.model_order')}</h3>
            <p class="text-muted mb-2" style="font-size:0.8rem">
              ${t('agents.detail.model_order_desc')}
            </p>
            ${models.length === 0
              ? html`<p class="text-muted" style="font-size:0.85rem">${t('agents.detail.no_models')}</p>`
              : html`
                <div class="table-responsive">
                  <table class="table table-sm agent-model-table mb-0">
                    <thead>
                      <tr>
                        <th>${t('agents.table.rank')}</th>
                        <th>${t('agents.table.strength')}</th>
                        <th>${t('agents.table.name')}</th>
                        <th>${t('agents.table.model_id')}</th>
                      </tr>
                    </thead>
                    <tbody>
                      ${models.map((m, i) => this._renderModelRow(m, i))}
                    </tbody>
                  </table>
                </div>
              `
            }
          </section>

          <section class="agent-section">
            <h3 class="agent-section-title">${t('agents.detail.prompt')}</h3>
            <div class="agent-prompt-body markdown-body">
              ${unsafeHTML(renderMarkdown(prompt))}
            </div>
          </section>
        </div>
      </div>
    `;
  }

  // ── Root render ───────────────────────────────────────────────────────────

  render() {
    return html`
      <div class="agents-page">
        ${this._detail
          ? this._renderDetail()
          : html`
            <div class="agents-page-header">
              <h2 class="llm-page-title">${t('agents.title')}</h2>
            </div>

            <div class="agent-info-banner">
              <div class="agent-info-banner-icon"><i class="bi bi-info-circle-fill"></i></div>
              <div class="agent-info-banner-body">
                <p class="mb-1">${unsafeHTML(t('agents.banner.title'))}</p>
                <p class="mb-0">${unsafeHTML(t('agents.banner.text'))}</p>
              </div>
            </div>

            ${this._renderList()}
          `
        }
      </div>
    `;
  }
}
