import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';
import { t }            from '../lib/i18n.js';
import { ConfigFormController, maybeT, propKeyId } from './shared/config-form.js';

const PAGE_ID  = 'system-agents';
const PER_PAGE = 20;

/** The overview tab: every agent's runs, interleaved. */
const ALL_TAB = '__all__';

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

/**
 * The background agents the instance runs, one tab per agent.
 *
 * **The tab is the agent, not the kind of information.** A tab holds an agent's
 * settings *and* its run history, because the question people actually arrive
 * with — "why did this do nothing last night?" — is answered half by the
 * schedule and half by the log. Splitting them into a "runs" tab and a
 * "settings" tab would put the two halves of every answer on opposite sides of
 * the page.
 *
 * **Two audiences on one page.** The run history is the caller's own and is
 * shown to everyone; the settings are instance-wide and shown only to an admin
 * (`can_configure`). Hiding the form is presentation only — the backend gates
 * both the listing and `PUT /api/config/{key}`.
 */
export class SystemAgentsPage extends LightElement {
  static properties = {
    _open:    { state: true },
    _agents:  { state: true },
    _canCfg:  { state: true },
    _tab:     { state: true },
    _items:   { state: true },
    _total:   { state: true },
    _page:    { state: true },
    _loading: { state: true },
    _error:   { state: true },
  };

  constructor() {
    super();
    this._open    = false;
    this._agents  = [];
    this._canCfg  = false;
    this._tab     = ALL_TAB;
    this._items   = [];
    this._total   = 0;
    this._page    = 1;
    this._loading = false;
    this._error   = null;
    this._form    = new ConfigFormController(() => this.requestUpdate());
  }

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === PAGE_ID;
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) this._loadAll();
    });
  }

  disconnectedCallback() {
    window.removeEventListener('locale-changed', this.__onLocaleChanged);
    super.disconnectedCallback();
  }

  async _loadAll() {
    await this._fetchAgents();
    await this._fetch(this._page);
  }

  async _fetchAgents() {
    try {
      const res = await fetch('/api/system-agents');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const data   = await res.json();
      this._agents = data.items ?? [];
      this._canCfg = !!data.can_configure;
      // A member gets no `config` at all, so there is nothing to seed.
      this._form.seedFromSets(this._agents.map(a => a.config).filter(Boolean));
    } catch (e) {
      // Non-fatal: without the agent list the page still shows the run log,
      // which is the half everyone can see.
      this._agents = [];
      this._canCfg = false;
    }
  }

  async _fetch(page) {
    this._loading = true;
    this._error   = null;
    try {
      const params = new URLSearchParams({ page, per_page: PER_PAGE });
      if (this._tab !== ALL_TAB) params.set('agent_id', this._tab);
      const res = await fetch(`/api/system-agents/runs?${params}`);
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

  _selectTab(id) {
    if (this._tab === id) return;
    this._tab  = id;
    this._page = 1;
    this._fetch(1);
  }

  _openSession(id) {
    if (id != null) window.location.hash = `session/${id}`;
  }

  get _totalPages() { return Math.max(1, Math.ceil(this._total / PER_PAGE)); }

  get _currentAgent() {
    return this._agents.find(a => a.id === this._tab) ?? null;
  }

  /** Server-supplied English unless the instance ships a translation. */
  _agentLabel(agent) {
    return {
      name:        maybeT(`system_agents.agent.${agent.id}.name`, agent.name),
      description: maybeT(`system_agents.agent.${agent.id}.desc`, agent.description),
    };
  }

  /// The agent's own counters. Rendered generically so a new system agent needs
  /// no change here: unknown keys fall back to the raw key name.
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

  _renderTabs() {
    if (this._agents.length === 0) return nothing;
    const tab = (id, label, icon) => html`
      <button class="sa-tab ${this._tab === id ? 'sa-tab--active' : ''}"
              @click=${() => this._selectTab(id)}>
        ${icon ? html`<i class="bi ${icon}"></i>` : nothing}${label}
      </button>`;

    return html`
      <div class="sa-tab-bar">
        ${tab(ALL_TAB, t('system_agents.tab.all'), 'bi-collection')}
        ${this._agents.map(a => tab(a.id, this._agentLabel(a).name, null))}
      </div>`;
  }

  /** The selected agent's description, plus its settings when the caller is an admin. */
  _renderAgentPanel() {
    const agent = this._currentAgent;
    if (!agent) return nothing;
    const label = this._agentLabel(agent);

    return html`
      <div class="sa-agent-panel">
        <p class="sa-agent-desc">${label.description}</p>
        ${this._canCfg && agent.config ? html`
          <div class="config-set sa-agent-config">
            <div class="config-set-header">
              <div class="config-set-name">${t('system_agents.settings')}</div>
            </div>
            ${this._form.renderRows(agent.config.properties, p => {
              const pk = propKeyId(p.key);
              return {
                name:        maybeT(`config.prop.${pk}.name`, p.name),
                description: maybeT(`config.prop.${pk}.desc`, p.description),
              };
            })}
          </div>` : nothing}
      </div>`;
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

    // The agent column is redundant once a single agent's tab is selected.
    const showAgent = this._tab === ALL_TAB;

    return html`
      <div class="sa-table-wrap">
        <table class="table table-sm sa-table">
          <thead>
            <tr>
              ${showAgent ? html`<th>${t('system_agents.table.agent')}</th>` : nothing}
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
                ${showAgent ? html`<td><span class="sa-agent">${r.agent_id}</span></td>` : nothing}
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
      <div class="sa-page">
        <div class="page-header">
          <div class="page-header-left">
            <h2 class="page-header-title"><i class="bi bi-robot"></i> ${t('system_agents.title')}</h2>
          </div>
          <div class="page-header-actions">
            <span class="page-header-count">${t('system_agents.total', { n: this._total })}</span>
            <button class="btn btn-sm btn-outline-secondary"
                    ?disabled=${this._loading}
                    @click=${() => this._loadAll()}>
              <i class="bi bi-arrow-clockwise"></i> ${t('system_agents.refresh')}
            </button>
          </div>
        </div>
        <div class="sa-body">
          <p class="sa-subtitle">${t('system_agents.subtitle')}</p>
          ${this._renderTabs()}
          ${this._renderAgentPanel()}
          ${this._renderTable()}
          ${this._renderPagination()}
        </div>
      </div>
    `;
  }
}
