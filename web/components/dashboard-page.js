import { html, nothing } from 'lit';
import { LightElement }  from '../lib/base.js';
import { t }             from '../lib/i18n.js';
import { InboxMixin }    from '../lib/inbox-mixin.js';

export class DashboardPage extends InboxMixin(LightElement) {

  static get properties() {
    return {
      ...super.properties,
      _open:         { state: true },
      _models:       { state: true },
      _plugins:      { state: true },
      _stats:        { state: true },
      _statsRange:   { state: true },
    };
  }

  constructor() {
    super();
    this._open         = false;
    this._models       = null;   // null = loading, [] = no models configured
    this._plugins      = null;
    this._pollTimer    = null;
    this._stats          = null;   // null = loading
    this._statsRange     = 'week';
    this._chartInstances = {};
    this._statsTimer     = null;
  }

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'dashboard';
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) {
        this._loadAll();
        this._loadStats();
        this._startPolling();
      } else {
        this._stopPolling();
      }
    });
  }

  disconnectedCallback() {
    window.removeEventListener('locale-changed', this.__onLocaleChanged);
    super.disconnectedCallback();
    this._stopPolling();
    this._destroyCharts();
  }

  updated(changed) {
    super.updated?.(changed);
    if (changed.has('_stats') && this._stats !== null) {
      requestAnimationFrame(() => this._initCharts());
    }
  }

  _startPolling() {
    this._stopPolling();
    this._pollTimer  = setInterval(() => this._loadAll(),   10_000);
    this._statsTimer = setInterval(() => this._loadStats(), 180_000);
  }

  _stopPolling() {
    if (this._pollTimer)  { clearInterval(this._pollTimer);  this._pollTimer  = null; }
    if (this._statsTimer) { clearInterval(this._statsTimer); this._statsTimer = null; }
  }

  async _loadAll() {
    await Promise.all([
      this._loadModels(),
      this._loadPlugins(),
      this._loadInbox(),
    ]);
  }

  async _loadModels() {
    try {
      const res = await fetch('/api/llm/models');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      this._models = await res.json();
    } catch {
      this._models = [];
    }
  }

  async _loadPlugins() {
    try {
      const res = await fetch('/api/plugins');
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      this._plugins = await res.json();
    } catch {
      this._plugins = [];
    }
  }

  async _loadStats() {
    try {
      const res = await fetch(`/api/stats/llm?range=${this._statsRange}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      this._stats = await res.json();
    } catch {
      this._stats = { daily: [], models: [] };
    }
  }

  async _setRange(range) {
    if (range === this._statsRange) return;
    this._statsRange = range;
    this._stats = null;
    await this._loadStats();
  }

  get _honchoActive() {
    return this._plugins?.some(p => p.id === 'honcho' && p.enabled && p.running) ?? false;
  }

  get _statusInfo() {
    if (this._models === null)       return { cls: 'loading', dot: false, icon: null,                    text: t('dashboard.status.loading') };
    if (this._models.length === 0)   return { cls: 'error',   dot: false, icon: 'bi-exclamation-circle-fill', text: t('dashboard.status.no_models') };
    if (this._models.some(m => m.status === 'healthy'))  return { cls: 'online',  dot: true,  icon: null,                    text: t('dashboard.status.online') };
    if (this._models.some(m => m.status === 'degraded')) return { cls: 'warn',    dot: true,  icon: 'bi-exclamation-triangle-fill', text: t('dashboard.status.degraded') };
    return { cls: 'error', dot: false, icon: 'bi-exclamation-circle-fill', text: t('dashboard.status.offline') };
  }

  get _guide() {
    return [
      { icon: 'bi-chat-dots-fill', title: t('dashboard.guide.chat.title'),      desc: t('dashboard.guide.chat.desc'),      color: '#d95d4e' },
      { icon: 'bi-inbox',          title: t('dashboard.guide.inbox.title'),     desc: t('dashboard.guide.inbox.desc'),     color: '#f59e0b' },
      { icon: 'bi-people',         title: t('dashboard.guide.agents.title'),    desc: t('dashboard.guide.agents.desc'),    color: '#8b5cf6' },
      { icon: 'bi-clock',          title: t('dashboard.guide.cron.title'),      desc: t('dashboard.guide.cron.desc'),      color: '#f97316' },
      { icon: 'bi-cpu',            title: t('dashboard.guide.models.title'),    desc: t('dashboard.guide.models.desc'),    color: '#10b981' },
      { icon: 'bi-plug',           title: t('dashboard.guide.providers.title'), desc: t('dashboard.guide.providers.desc'), color: '#06b6d4' },
      { icon: 'bi-shield-check',   title: t('dashboard.guide.security.title'),  desc: t('dashboard.guide.security.desc'),  color: '#ef4444' },
    ];
  }

  _nav(page) {
    const url = '#' + page;
    history.pushState({ page }, '', url);
    window.dispatchEvent(new CustomEvent('llm-page-change', { detail: { page } }));
  }

  // ── Charts ────────────────────────────────────────────────────────────────

  _destroyCharts() {
    for (const c of Object.values(this._chartInstances)) {
      c.destroy();
    }
    this._chartInstances = {};
  }

  _shortModelName(name) {
    return name
      .replace(/^claude-/, '')
      .replace(/^gpt-/, '')
      .replace(/-\d{8}$/, '');
  }

  get _periodLabel() {
    return { hour: t('dashboard.stats.per_min'), day: t('dashboard.stats.per_hour'), week: t('dashboard.stats.per_day'), month: t('dashboard.stats.per_day') }[this._statsRange] ?? t('dashboard.stats.per_day');
  }

  // Generates the full sequence of expected slots for the current range and
  // merges with backend data, filling missing slots with zeros.
  _fillGaps(daily) {
    const now   = new Date();
    const pad   = n => String(n).padStart(2, '0');
    const slots = [];

    if (this._statsRange === 'hour') {
      for (let i = 59; i >= 0; i--) {
        const d = new Date(now - i * 60_000);
        slots.push(`${pad(d.getHours())}:${pad(d.getMinutes())}`);
      }
    } else if (this._statsRange === 'day') {
      for (let i = 23; i >= 0; i--) {
        const d = new Date(now - i * 3_600_000);
        slots.push(`${pad(d.getMonth()+1)}-${pad(d.getDate())} ${pad(d.getHours())}:00`);
      }
    } else {
      const count = this._statsRange === 'week' ? 7 : 30;
      for (let i = count - 1; i >= 0; i--) {
        const d = new Date(now - i * 86_400_000);
        slots.push(`${d.getFullYear()}-${pad(d.getMonth()+1)}-${pad(d.getDate())}`);
      }
    }

    const index = new Map(daily.map(d => [d.day, d]));
    const zero  = { requests: 0, input_tokens: 0, output_tokens: 0, cache_read_tokens: 0, avg_duration_ms: 0 };
    return slots.map(s => ({ day: s, ...(index.get(s) ?? zero) }));
  }

  _initCharts() {
    if (!window.Chart || !this._stats) return;
    this._destroyCharts();

    const dark      = document.documentElement.getAttribute('data-bs-theme') === 'dark';
    const gridColor = dark ? 'rgba(255,255,255,0.07)' : 'rgba(0,0,0,0.06)';
    const textColor = dark ? '#adb5bd' : '#6c757d';
    const isHour    = this._statsRange === 'hour';

    const filled = this._fillGaps(this._stats.daily);
    const days   = filled.map(d => d.day);
    const req    = filled.map(d => d.requests);
    const inp    = filled.map(d => d.input_tokens);
    const out    = filled.map(d => d.output_tokens);
    const cache  = filled.map(d => d.cache_read_tokens);
    // null for empty slots so the latency line doesn't touch zero where there were no requests
    const lat    = filled.map(d => d.requests > 0 ? Math.round(d.avg_duration_ms) : null);
    const models = this._stats.models;

    const axisDefaults = () => ({
      ticks:  { color: textColor, font: { size: 11 } },
      grid:   { color: gridColor },
      border: { color: gridColor },
    });

    const xAxis = () => ({
      ...axisDefaults(),
      ticks: {
        color:         textColor,
        font:          { size: 11 },
        maxTicksLimit: isHour ? 7 : 15,
        maxRotation:   0,
        minRotation:   0,
      },
    });

    const baseOpts = (extraPlugins = {}) => ({
      responsive:          true,
      maintainAspectRatio: false,
      animation:           { duration: 300 },
      plugins: {
        legend: { display: false },
        ...extraPlugins,
      },
      scales: {
        x: xAxis(),
        y: { ...axisDefaults(), beginAtZero: true },
      },
    });

    const barDs = (data, color) => ({
      data, backgroundColor: color, borderRadius: 4, borderSkipped: false,
    });

    const lineDs = (data, borderColor, bgColor, opts = {}) => ({
      data, borderColor, backgroundColor: bgColor,
      fill: true, tension: 0.3, pointRadius: 0, borderWidth: 2,
      ...opts,
    });

    const type = isHour ? 'line' : 'bar';
    const get  = id => this.querySelector(`#${id}`);

    // Requests
    const c1 = get('chart-requests');
    if (c1) this._chartInstances.requests = new Chart(c1, {
      type,
      data: {
        labels:   days,
        datasets: [isHour
          ? lineDs(req, '#3b82f6', 'rgba(59,130,246,0.12)')
          : barDs(req, '#3b82f6')],
      },
      options: baseOpts(),
    });

    // Tokens
    const c2 = get('chart-tokens');
    if (c2) this._chartInstances.tokens = new Chart(c2, {
      type,
      data: {
        labels:   days,
        datasets: isHour ? [
          lineDs(inp,   '#3b82f6', 'rgba(59,130,246,0)',  { fill: false, label: t('dashboard.stats.chart.input')   }),
          lineDs(out,   '#10b981', 'rgba(16,185,129,0)',  { fill: false, label: t('dashboard.stats.chart.output')  }),
          lineDs(cache, '#f59e0b', 'rgba(245,158,11,0)',  { fill: false, label: t('dashboard.stats.chart.cached')  }),
        ] : (() => {
          const nonCached = inp.map((v, i) => Math.max(0, v - (cache[i] ?? 0)));
          return [
            { label: t('dashboard.stats.chart.cached'),     data: cache,     backgroundColor: '#f59e0b', stack: 'tok', borderSkipped: false },
            { label: t('dashboard.stats.chart.non_cached'), data: nonCached, backgroundColor: '#3b82f6', stack: 'tok', borderSkipped: false },
            { label: t('dashboard.stats.chart.output'),     data: out,        backgroundColor: '#10b981', stack: 'tok', borderRadius: 4, borderSkipped: false },
          ];
        })(),
      },
      options: baseOpts({
        legend: {
          display: true,
          labels:  { color: textColor, boxWidth: 10, font: { size: 11 } },
        },
        tooltip: {
          callbacks: {
            footer(items) {
              const idx   = items[0]?.dataIndex;
              const total = inp[idx] ?? 0;
              if (!total) return '';
              const pct = Math.round((cache[idx] ?? 0) / total * 100);
              return t('dashboard.stats.chart.cache_hit', { pct });
            },
          },
        },
      }),
    });

    // Latency
    const c3 = get('chart-latency');
    if (c3) this._chartInstances.latency = new Chart(c3, {
      type,
      data: {
        labels:   days,
        datasets: [isHour
          ? lineDs(lat, '#8b5cf6', 'rgba(139,92,246,0.12)', { spanGaps: false })
          : barDs(lat.map(v => v ?? 0), '#8b5cf6')],
      },
      options: baseOpts(),
    });

    // Models — always horizontal bar
    const c4 = get('chart-models');
    if (c4) this._chartInstances.models = new Chart(c4, {
      type: 'bar',
      data: {
        labels:   models.map(m => this._shortModelName(m.model_name)),
        datasets: [{
          data:            models.map(m => m.requests),
          backgroundColor: ['#3b82f6','#10b981','#f59e0b','#8b5cf6','#ef4444','#06b6d4'],
          borderRadius:    4,
          borderSkipped:   false,
        }],
      },
      options: {
        ...baseOpts(),
        indexAxis: 'y',
        scales: {
          x: { ...axisDefaults(), beginAtZero: true },
          y: { ...axisDefaults(), ticks: { color: textColor, font: { size: 10 } } },
        },
      },
    });
  }

  // ── Render ────────────────────────────────────────────────────────────────

  _renderStats() {
    if (this._stats === null) {
      return html`<div class="home-stats-loading"><i class="bi bi-hourglass-split"></i> ${t('dashboard.stats.loading')}</div>`;
    }

    const empty = this._stats.daily.length === 0 && this._stats.models.length === 0;
    if (empty) {
      return html`
        <div class="home-stats-empty">
          <i class="bi bi-bar-chart"></i>
          <span>${t('dashboard.stats.empty')}</span>
        </div>
      `;
    }

    return html`
      <div class="home-stats-grid">
        <div class="home-stat-card">
          <div class="home-stat-card-title">${t('dashboard.stats.requests', { per: this._periodLabel })}</div>
          <div class="home-stat-canvas-wrap"><canvas id="chart-requests"></canvas></div>
        </div>
        <div class="home-stat-card">
          <div class="home-stat-card-title">${t('dashboard.stats.tokens', { per: this._periodLabel })}</div>
          <div class="home-stat-canvas-wrap"><canvas id="chart-tokens"></canvas></div>
        </div>
        <div class="home-stat-card">
          <div class="home-stat-card-title">${t('dashboard.stats.latency')}</div>
          <div class="home-stat-canvas-wrap"><canvas id="chart-latency"></canvas></div>
        </div>
        <div class="home-stat-card">
          <div class="home-stat-card-title">${t('dashboard.stats.models')}</div>
          <div class="home-stat-canvas-wrap"><canvas id="chart-models"></canvas></div>
        </div>
      </div>
    `;
  }

  render() {
    const st         = this._statusInfo;
    const noModels   = this._models !== null && this._models.length === 0;
    const approvals  = this._inboxData?.approvals      ?? [];
    const clarifs    = this._inboxData?.clarifications ?? [];
    const elicits    = this._inboxData?.elicitations   ?? [];
    const inboxTotal = approvals.length + clarifs.length + elicits.length;

    return html`
      <div class="home-page">

        <!-- ── Hero ── -->
        <div class="home-hero">
          <div class="home-hero-image">
            <img src="/assets/icons/icon-1024.png" alt=${t('chat.title')} />
          </div>
          <div class="home-hero-text">
            <h1 class="home-hero-title">${t('chat.title')}</h1>
            <p class="home-hero-desc">${t('dashboard.hero.subtitle')}</p>
            <div class="home-hero-status home-hero-status--${st.cls}">
              ${st.dot  ? html`<span class="home-hero-dot"></span>` : nothing}
              ${st.icon ? html`<i class="bi ${st.icon}"></i>` : nothing}
              <span>${st.text}</span>
            </div>
          </div>
        </div>

        <!-- ── No-models banner ── -->
        ${noModels ? html`
          <div class="home-banner home-banner--error">
            <div class="home-banner-icon"><i class="bi bi-cpu-fill"></i></div>
            <div class="home-banner-body">
              <strong>${t('dashboard.banner.no_models.title')}</strong>
              ${t('dashboard.banner.no_models.desc')}
            </div>
            <button class="btn btn-sm btn-danger" @click=${() => this._nav('providers')}>
              ${t('dashboard.banner.no_models.action')}
            </button>
          </div>
        ` : nothing}

        <!-- ── LLM Stats ── -->
        <div class="home-section-title">
          <i class="bi bi-bar-chart-fill"></i>
          <span>${t('dashboard.section.stats')}</span>
          <div class="home-stats-range ms-auto">
            ${[['hour', t('dashboard.stats.range.hour')], ['day', t('dashboard.stats.range.day')], ['week', t('dashboard.stats.range.week')], ['month', t('dashboard.stats.range.month')]].map(([r, label]) => html`
              <button class="home-stats-range-btn ${this._statsRange === r ? 'active' : ''}"
                      @click=${() => this._setRange(r)}>${label}</button>
            `)}
          </div>
        </div>
        ${this._renderStats()}

        <!-- ── Pending inbox ── -->
        <div class="home-section-title">
          <i class="bi bi-inbox"></i>
          <span>${t('dashboard.section.pending')}</span>
          ${inboxTotal > 0 ? html`<span class="badge bg-danger">${inboxTotal}</span>` : nothing}
          <button class="inbox-refresh-btn ms-auto" title=${t('dashboard.refresh')} @click=${() => this._loadInbox()}>
            <i class="bi bi-arrow-clockwise"></i>
          </button>
        </div>
        ${this._renderInboxSection()}

        <!-- ── Honcho tip ── -->
        ${!this._honchoActive ? html`
          <div class="home-tip">
            <div class="home-tip-icon"><i class="bi bi-lightbulb-fill"></i></div>
            <div class="home-tip-body">
              <strong>${t('dashboard.tip.honcho.title')}</strong>
              <span>${t('dashboard.tip.honcho.desc')}</span>
            </div>
          </div>
        ` : nothing}

        <!-- ── Quick guide ── -->
        <div class="home-section-title">
          <i class="bi bi-map"></i>
          <span>${t('dashboard.section.guide')}</span>
        </div>
        <div class="home-guide">
          ${this._guide.map(s => html`
            <div class="home-card" style="--home-card-color: ${s.color}">
              <div class="home-card-icon">
                <i class="bi ${s.icon}"></i>
              </div>
              <div class="home-card-body">
                <h6 class="home-card-title">${s.title}</h6>
                <p class="home-card-desc">${s.desc}</p>
              </div>
            </div>
          `)}
        </div>
      </div>
    `;
  }
}
