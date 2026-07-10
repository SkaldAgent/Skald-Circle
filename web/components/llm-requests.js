import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';

const PAGE_ID   = 'llm-requests';
const PAGE_SIZE = 20;

function formatDate(iso) {
  if (!iso) return '—';
  return new Date(iso).toLocaleString(undefined, {
    day: '2-digit', month: '2-digit', year: '2-digit',
    hour: '2-digit', minute: '2-digit',
  });
}

function fmtTokens(n) {
  if (n == null) return '—';
  if (n >= 1000) return (n / 1000).toFixed(1) + 'k';
  return String(n);
}

function cacheHitPct(item) {
  if (item.cache_read_tokens == null || !item.input_tokens) return '—';
  return (item.cache_read_tokens / item.input_tokens * 100).toFixed(0) + '%';
}

function cacheTooltip(item) {
  const parts = [];
  if (item.cache_read_tokens != null) parts.push(`read: ${item.cache_read_tokens.toLocaleString()} tk`);
  if (item.cache_creation_tokens != null) parts.push(`write: ${item.cache_creation_tokens.toLocaleString()} tk`);
  return parts.length ? parts.join(' | ') : '';
}

export class LlmRequestsPage extends LightElement {
  static properties = {
    _open:      { state: true },
    _items:     { state: true },
    _total:     { state: true },
    _page:      { state: true },
    _loading:   { state: true },
    _error:     { state: true },
    _agentId:   { state: true },
    _source:    { state: true },
    _from:      { state: true },
    _to:        { state: true },
    _applied:   { state: true },
    _detailId:  { state: true },
  };

  constructor() {
    super();
    this._open      = false;
    this._items     = [];
    this._total     = 0;
    this._page      = 1;
    this._loading   = false;
    this._error     = null;
    this._agentId   = '';
    this._source    = '';
    this._from      = '';
    this._to        = '';
    this._applied   = {};
    this._detailId  = null;
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === PAGE_ID;
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) {
        const id = this._idFromHash();
        this._detailId = id;
        if (id == null && this._items.length === 0) this._fetch(1);
      }
    });
  }

  _idFromHash() {
    const parts = location.hash.replace('#', '').split('/');
    if (parts[0] === PAGE_ID && parts[1]) {
      const n = Number(parts[1]);
      return isNaN(n) ? null : n;
    }
    return null;
  }

  _openDetail(id) {
    this._detailId = id;
    history.pushState({}, '', `#${PAGE_ID}/${id}`);
  }

  _back() {
    this._detailId = null;
    history.pushState({}, '', `#${PAGE_ID}`);
    if (this._items.length === 0) this._fetch(1);
  }

  // ── List fetching ────────────────────────────────────────────────────────────

  async _fetch(page) {
    this._loading = true;
    this._error   = null;
    const params  = new URLSearchParams({ page });
    if (this._agentId) params.set('agent_id', this._agentId);
    if (this._source)  params.set('source',   this._source);
    if (this._from)    params.set('from',      this._from);
    if (this._to)      params.set('to',        this._to);
    this._applied = { agentId: this._agentId, source: this._source, from: this._from, to: this._to };
    try {
      const res = await fetch(`/api/dev/llm-requests?${params}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data  = await res.json();
      this._items = data.items;
      this._total = data.total;
      this._page  = data.page;
    } catch (e) {
      this._error = e.message;
    } finally {
      this._loading = false;
    }
  }

  _apply() { this._fetch(1); }

  _reset() {
    this._agentId = '';
    this._source  = '';
    this._from    = '';
    this._to      = '';
    this._fetch(1);
  }

  get _totalPages() { return Math.max(1, Math.ceil(this._total / PAGE_SIZE)); }

  // ── Renders ──────────────────────────────────────────────────────────────────

  _renderFilters() {
    return html`
      <div class="llmr-filters">
        <div class="llmr-filter-group">
          <label class="llmr-filter-label">Agent ID</label>
          <input class="form-control form-control-sm" type="text"
                 placeholder="e.g. main"
                 .value=${this._agentId}
                 @input=${e => this._agentId = e.target.value}
                 @keydown=${e => e.key === 'Enter' && this._apply()} />
        </div>
        <div class="llmr-filter-group">
          <label class="llmr-filter-label">Source</label>
          <input class="form-control form-control-sm" type="text"
                 placeholder="e.g. web, tic, cron"
                 .value=${this._source}
                 @input=${e => this._source = e.target.value}
                 @keydown=${e => e.key === 'Enter' && this._apply()} />
        </div>
        <div class="llmr-filter-group">
          <label class="llmr-filter-label">From</label>
          <input class="form-control form-control-sm" type="date"
                 .value=${this._from}
                 @change=${e => this._from = e.target.value} />
        </div>
        <div class="llmr-filter-group">
          <label class="llmr-filter-label">To</label>
          <input class="form-control form-control-sm" type="date"
                 .value=${this._to}
                 @change=${e => this._to = e.target.value} />
        </div>
        <div class="llmr-filter-actions">
          <button class="btn btn-sm btn-primary" @click=${() => this._apply()}
                  ?disabled=${this._loading}>
            Apply
          </button>
          <button class="btn btn-sm btn-outline-secondary" @click=${() => this._reset()}
                  ?disabled=${this._loading}>
            Reset
          </button>
        </div>
      </div>
    `;
  }

  _renderTable() {
    if (this._loading) return html`
      <div class="llmr-state">
        <div class="spinner-border spinner-border-sm text-secondary" role="status"></div>
        <span>Loading…</span>
      </div>
    `;
    if (this._error) return html`
      <div class="llmr-state llmr-state--error">
        <i class="bi bi-exclamation-circle"></i>
        <span>${this._error}</span>
      </div>
    `;
    if (this._items.length === 0) return html`
      <div class="llmr-state">
        <i class="bi bi-inbox"></i>
        <span>No requests found.</span>
      </div>
    `;

    return html`
      <div class="llmr-table-wrap">
        <table class="table table-sm llmr-table">
          <thead>
            <tr>
              <th>Agent</th>
              <th>Source</th>
              <th>Model</th>
              <th>Date</th>
              <th class="text-end">In tokens</th>
              <th class="text-end">Out tokens</th>
              <th class="text-end">Cache hit</th>
              <th class="text-end">ms</th>
            </tr>
          </thead>
          <tbody>
            ${this._items.map(r => html`
              <tr class="${r.error_text ? 'llmr-row--error' : ''} llmr-row--clickable"
                  @click=${() => this._openDetail(r.id)}>
                <td><span class="llmr-badge-agent">${r.agent_id ?? '—'}</span></td>
                <td><span class="llmr-badge-source">${r.source ?? '—'}</span></td>
                <td class="llmr-model">${r.model_name}</td>
                <td class="llmr-date">${formatDate(r.created_at)}</td>
                <td class="text-end llmr-num">${fmtTokens(r.input_tokens)}</td>
                <td class="text-end llmr-num">${fmtTokens(r.output_tokens)}</td>
                <td class="text-end llmr-num ${r.cache_read_tokens > 0 ? 'llmr-cache-hit' : ''}"
                    title=${cacheTooltip(r)}>
                  ${cacheHitPct(r)}
                </td>
                <td class="text-end llmr-num">${r.duration_ms}</td>
              </tr>
              ${r.error_text ? html`
                <tr class="llmr-row--error-detail">
                  <td colspan="8">
                    <i class="bi bi-exclamation-triangle-fill"></i> ${r.error_text}
                  </td>
                </tr>
              ` : nothing}
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
      <div class="llmr-pagination">
        <button class="btn btn-sm btn-outline-secondary" ?disabled=${cur <= 1}
                @click=${() => this._fetch(cur - 1)}>
          <i class="bi bi-chevron-left"></i>
        </button>
        <span class="llmr-page-info">Page ${cur} of ${pages} &mdash; ${this._total} results</span>
        <button class="btn btn-sm btn-outline-secondary" ?disabled=${cur >= pages}
                @click=${() => this._fetch(cur + 1)}>
          <i class="bi bi-chevron-right"></i>
        </button>
      </div>
    `;
  }

  render() {
    if (this._detailId != null) {
      return html`
        <llm-request-detail
          .detailId=${this._detailId}
          @detail-back=${() => this._back()}>
        </llm-request-detail>
      `;
    }

    return html`
      <div class="llmr-page">
        <div class="llmr-header">
          <h2 class="llmr-title"><i class="bi bi-journal-code"></i> LLM Requests</h2>
          <span class="llmr-total-badge">${this._total} rows</span>
        </div>
        ${this._renderFilters()}
        ${this._renderTable()}
        ${this._renderPagination()}
      </div>
    `;
  }
}
