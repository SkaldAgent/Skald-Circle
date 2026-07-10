import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';

const PAGE_ID   = 'tic';
const PER_PAGE  = 20;

function formatDate(iso) {
  if (!iso) return '—';
  return new Date(iso).toLocaleString(undefined, {
    day: '2-digit', month: '2-digit', year: '2-digit',
    hour: '2-digit', minute: '2-digit', second: '2-digit',
  });
}

function formatDateShort(iso) {
  if (!iso) return '—';
  return new Date(iso).toLocaleString(undefined, {
    day: '2-digit', month: '2-digit', year: '2-digit',
    hour: '2-digit', minute: '2-digit',
  });
}

export class TicSessionsPage extends LightElement {
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
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === PAGE_ID;
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open && this._items.length === 0) this._fetch(1);
    });
  }

  async _fetch(page) {
    this._loading = true;
    this._error   = null;
    try {
      const params = new URLSearchParams({ source: 'tic', page, per_page: PER_PAGE });
      const res    = await fetch(`/api/sessions?${params}`);
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
    window.location.hash = `session/${id}`;
  }

  get _totalPages() { return Math.max(1, Math.ceil(this._total / PER_PAGE)); }

  _renderTable() {
    if (this._loading) return html`
      <div class="tic-state">
        <div class="spinner-border spinner-border-sm text-secondary" role="status"></div>
        <span>Loading…</span>
      </div>
    `;
    if (this._error) return html`
      <div class="tic-state tic-state--error">
        <i class="bi bi-exclamation-circle"></i>
        <span>${this._error}</span>
      </div>
    `;
    if (this._items.length === 0) return html`
      <div class="tic-state">
        <i class="bi bi-inbox"></i>
        <span>No TIC sessions found.</span>
      </div>
    `;

    return html`
      <div class="tic-table-wrap">
        <table class="table table-sm tic-table">
          <thead>
            <tr>
              <th>#</th>
              <th>Agent</th>
              <th>Started</th>
              <th class="text-end">Messages</th>
              <th>Last activity</th>
            </tr>
          </thead>
          <tbody>
            ${this._items.map(r => html`
              <tr class="tic-row--clickable" @click=${() => this._openSession(r.id)}>
                <td class="tic-id">${r.id}</td>
                <td><span class="tic-agent">${r.agent_id ?? '—'}</span></td>
                <td class="tic-date">${formatDateShort(r.created_at)}</td>
                <td class="text-end tic-num">${r.message_count}</td>
                <td class="tic-date">${formatDate(r.last_message_at)}</td>
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
      <div class="tic-pagination">
        <button class="btn btn-sm btn-outline-secondary" ?disabled=${cur <= 1}
                @click=${() => this._fetch(cur - 1)}>
          <i class="bi bi-chevron-left"></i>
        </button>
        <span class="tic-page-info">Page ${cur} of ${pages} &mdash; ${this._total} sessions</span>
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
        .tic-page {
          display: flex;
          flex-direction: column;
          flex: 1;
          min-height: 0;
          padding: 1.5rem;
          overflow-y: auto;
        }
        .tic-header {
          display: flex;
          align-items: baseline;
          gap: 0.75rem;
          margin-bottom: 1.25rem;
        }
        .tic-title {
          font-size: 1.2rem;
          font-weight: 600;
          margin: 0;
        }
        .tic-total-badge {
          font-size: 0.75rem;
          color: var(--bs-secondary-color);
          background: var(--bs-tertiary-bg);
          border: 1px solid var(--bs-border-color);
          border-radius: 1rem;
          padding: 0.1rem 0.6rem;
        }
        .tic-refresh-btn {
          margin-left: auto;
        }
        .tic-table-wrap {
          border: 1px solid var(--bs-border-color);
          border-radius: 0.5rem;
          overflow: hidden;
        }
        .tic-table {
          margin-bottom: 0;
        }
        .tic-row--clickable {
          cursor: pointer;
        }
        .tic-row--clickable:hover td {
          background: var(--bs-tertiary-bg);
        }
        .tic-id {
          font-family: monospace;
          font-size: 0.82rem;
          color: var(--bs-secondary-color);
          width: 4rem;
        }
        .tic-agent {
          font-family: monospace;
          font-size: 0.82rem;
        }
        .tic-date {
          font-size: 0.82rem;
          color: var(--bs-secondary-color);
          white-space: nowrap;
        }
        .tic-num {
          font-variant-numeric: tabular-nums;
          font-size: 0.85rem;
        }
        .tic-state {
          display: flex;
          align-items: center;
          gap: 0.5rem;
          padding: 3rem;
          justify-content: center;
          color: var(--bs-secondary-color);
          font-size: 0.9rem;
        }
        .tic-state--error { color: var(--bs-danger); }
        .tic-pagination {
          display: flex;
          align-items: center;
          gap: 0.75rem;
          margin-top: 1rem;
          justify-content: center;
        }
        .tic-page-info {
          font-size: 0.82rem;
          color: var(--bs-secondary-color);
        }
      </style>

      <div class="tic-page">
        <div class="tic-header">
          <h2 class="tic-title"><i class="bi bi-bell"></i> TIC Sessions</h2>
          <span class="tic-total-badge">${this._total} total</span>
          <button class="btn btn-sm btn-outline-secondary tic-refresh-btn"
                  ?disabled=${this._loading}
                  @click=${() => this._fetch(this._page)}>
            <i class="bi bi-arrow-clockwise"></i> Refresh
          </button>
        </div>
        ${this._renderTable()}
        ${this._renderPagination()}
      </div>
    `;
  }
}
