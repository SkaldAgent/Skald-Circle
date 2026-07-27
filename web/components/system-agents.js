import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';
import { t }            from '../lib/i18n.js';

const PAGE_ID  = 'system-agents';
const PER_PAGE = 20;

function formatDate(iso) {
  if (!iso) return '—';
  return new Date(iso).toLocaleString(undefined, {
    day: '2-digit', month: '2-digit', year: '2-digit',
    hour: '2-digit', minute: '2-digit',
  });
}

function formatDuration(ms) {
  if (ms == null) return '—';
  if (ms < 1000) return `${ms} ms`;
  const s = ms / 1000;
  if (s < 60) return `${s.toFixed(1)} s`;
  const m = Math.floor(s / 60);
  return `${m}m ${Math.round(s % 60)}s`;
}

const STATUS_ICON = {
  running:   'bi-arrow-repeat',
  completed: 'bi-check-circle',
  failed:    'bi-exclamation-circle',
  cancelled: 'bi-slash-circle',
};

export class SystemAgentsPage extends LightElement {
  static properties = {
    _open:    { state: true },
    _items:   { state: true },
    _total:   { state: true },
    _page:    { state: true },
    _loading: { state: true },
    _error:   { state: true },
  };

  constructor() {
    super();
    this._open    = false;
    this._items   = [];
    this._total   = 0;
    this._page    = 1;
    this._loading = false;
    this._error   = null;
  }

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === PAGE_ID;
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) this._fetch(this._page);
    });
  }

  disconnectedCallback() {
    window.removeEventListener('locale-changed', this.__onLocaleChanged);
    super.disconnectedCallback();
  }

  async _fetch(page) {
    this._loading = true;
    this._error   = null;
    try {
      const params = new URLSearchParams({ page, per_page: PER_PAGE });
      const res    = await fetch(`/api/system-agents/runs?${params}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data    = await res.json();
      this._items   = data.items;
      this._total   = data.total;
      this._page    = data.page;
    } catch (e) {
      this._error = e.message;
    } finally {
      this._loading = false;
    }
  }

  _openSession(id) {
    if (id != null) window.location.hash = `session/${id}`;
  }

  get _totalPages() { return Math.max(1, Math.ceil(this._total / PER_PAGE)); }

  /// The agent's own counters. Rendered generically so a second system agent
  /// needs no change here: unknown keys fall back to the raw key name.
  _renderStats(stats) {
    if (!stats || typeof stats !== 'object') return '—';
    const parts = Object.entries(stats)
      .filter(([, v]) => v != null)
      .map(([k, v]) => {
        const label = t(`system_agents.stat.${k}`);
        return `${v} ${label.startsWith('system_agents.') ? k.replace(/_/g, ' ') : label}`;
      });
    return parts.length ? parts.join(' · ') : '—';
  }

  _renderTable() {
    if (this._loading) return html`
      <div class="sa-state">
        <div class="spinner-border spinner-border-sm text-secondary" role="status"></div>
        <span>${t('system_agents.loading')}</span>
      </div>
    `;
    if (this._error) return html`
      <div class="sa-state sa-state--error">
        <i class="bi bi-exclamation-circle"></i>
        <span>${this._error}</span>
      </div>
    `;
    if (this._items.length === 0) return html`
      <div class="sa-state sa-state--empty">
        <i class="bi bi-robot"></i>
        <span>${t('system_agents.empty')}</span>
        <small>${t('system_agents.empty_hint')}</small>
      </div>
    `;

    return html`
      <div class="sa-table-wrap">
        <table class="table table-sm sa-table">
          <thead>
            <tr>
              <th>${t('system_agents.table.agent')}</th>
              <th>${t('system_agents.table.started')}</th>
              <th>${t('system_agents.table.status')}</th>
              <th class="text-end">${t('system_agents.table.duration')}</th>
              <th>${t('system_agents.table.result')}</th>
            </tr>
          </thead>
          <tbody>
            ${this._items.map(r => html`
              <tr class=${r.session_id != null ? 'sa-row--clickable' : ''}
                  @click=${() => this._openSession(r.session_id)}>
                <td><span class="sa-agent">${r.agent_id}</span></td>
                <td class="sa-date">${formatDate(r.started_at)}</td>
                <td>
                  <span class="sa-status sa-status--${r.status}">
                    <i class="bi ${STATUS_ICON[r.status] ?? 'bi-question-circle'}"></i>
                    ${t(`system_agents.status.${r.status}`)}
                  </span>
                </td>
                <td class="text-end sa-num">${formatDuration(r.duration_ms)}</td>
                <td class="sa-result">
                  ${r.error
                    ? html`<span class="sa-error" title=${r.error}>${r.error}</span>`
                    : this._renderStats(r.stats)}
                </td>
              </tr>
            `)}
          </tbody>
        </table>
      </div>
    `;
  }

  _renderPagination() {
    if (this._totalPages <= 1) return nothing;
    const pages = this._totalPages;
    const cur   = this._page;
    return html`
      <div class="sa-pagination">
        <button class="btn btn-sm btn-outline-secondary" ?disabled=${cur <= 1}
                @click=${() => this._fetch(cur - 1)}>
          <i class="bi bi-chevron-left"></i>
        </button>
        <span class="sa-page-info">${t('system_agents.pagination', { cur, pages, total: this._total })}</span>
        <button class="btn btn-sm btn-outline-secondary" ?disabled=${cur >= pages}
                @click=${() => this._fetch(cur + 1)}>
          <i class="bi bi-chevron-right"></i>
        </button>
      </div>
    `;
  }

  render() {
    return html`
      <style>
        .sa-page {
          display: flex;
          flex-direction: column;
          flex: 1;
          min-height: 0;
          width: 100%;
          padding: 1.5rem;
          overflow-y: auto;
        }
        .sa-header {
          display: flex;
          align-items: baseline;
          gap: 0.75rem;
          margin-bottom: 0.35rem;
        }
        .sa-title {
          font-size: 1.2rem;
          font-weight: 600;
          margin: 0;
        }
        .sa-total-badge {
          font-size: 0.75rem;
          color: var(--bs-secondary-color);
          background: var(--bs-tertiary-bg);
          border: 1px solid var(--bs-border-color);
          border-radius: 1rem;
          padding: 0.1rem 0.6rem;
        }
        .sa-refresh-btn { margin-left: auto; }
        .sa-subtitle {
          font-size: 0.85rem;
          color: var(--bs-secondary-color);
          margin-bottom: 1.25rem;
          max-width: 65ch;
        }
        .sa-table-wrap {
          border: 1px solid var(--bs-border-color);
          border-radius: 0.5rem;
          overflow-x: auto;
        }
        .sa-table { margin-bottom: 0; }
        .sa-row--clickable { cursor: pointer; }
        .sa-row--clickable:hover td { background: var(--bs-tertiary-bg); }
        .sa-agent {
          font-family: monospace;
          font-size: 0.82rem;
        }
        .sa-date {
          font-size: 0.82rem;
          color: var(--bs-secondary-color);
          white-space: nowrap;
        }
        .sa-num {
          font-variant-numeric: tabular-nums;
          font-size: 0.85rem;
          white-space: nowrap;
        }
        .sa-status {
          display: inline-flex;
          align-items: center;
          gap: 0.3rem;
          font-size: 0.8rem;
          white-space: nowrap;
        }
        .sa-status--completed { color: var(--bs-success); }
        .sa-status--failed    { color: var(--bs-danger); }
        .sa-status--running   { color: var(--bs-secondary-color); }
        .sa-status--cancelled { color: var(--bs-secondary-color); }
        .sa-result {
          font-size: 0.82rem;
          color: var(--bs-secondary-color);
        }
        .sa-error {
          color: var(--bs-danger);
          display: inline-block;
          max-width: 40ch;
          overflow: hidden;
          text-overflow: ellipsis;
          white-space: nowrap;
          vertical-align: bottom;
        }
        .sa-state {
          display: flex;
          flex-direction: column;
          align-items: center;
          gap: 0.4rem;
          padding: 3rem;
          justify-content: center;
          color: var(--bs-secondary-color);
          font-size: 0.9rem;
        }
        .sa-state--empty i { font-size: 1.6rem; opacity: 0.6; }
        .sa-state--error { color: var(--bs-danger); flex-direction: row; }
        .sa-pagination {
          display: flex;
          align-items: center;
          gap: 0.75rem;
          margin-top: 1rem;
          justify-content: center;
        }
        .sa-page-info {
          font-size: 0.82rem;
          color: var(--bs-secondary-color);
        }
      </style>

      <div class="sa-page">
        <div class="sa-header">
          <h2 class="sa-title"><i class="bi bi-robot"></i> ${t('system_agents.title')}</h2>
          <span class="sa-total-badge">${t('system_agents.total', { n: this._total })}</span>
          <button class="btn btn-sm btn-outline-secondary sa-refresh-btn"
                  ?disabled=${this._loading}
                  @click=${() => this._fetch(this._page)}>
            <i class="bi bi-arrow-clockwise"></i> ${t('system_agents.refresh')}
          </button>
        </div>
        <p class="sa-subtitle">${t('system_agents.subtitle')}</p>
        ${this._renderTable()}
        ${this._renderPagination()}
      </div>
    `;
  }
}
