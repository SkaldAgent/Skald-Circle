import { html, nothing } from 'lit';
import { LightElement }  from '../lib/base.js';
import { InboxMixin }    from '../lib/inbox-mixin.js';

const GUIDE = [
  {
    icon:  'bi-chat-dots-fill',
    title: 'Copilot',
    desc:  'The chat panel on the right knows everything — ask it to run agents, enable plugins, write code, or search the web.',
    color: '#0d6efd',
  },
  {
    icon:  'bi-inbox',
    title: 'Inbox',
    desc:  'Pending approvals and agent questions that need your input before background tasks can continue.',
    color: '#f59e0b',
  },
  {
    icon:  'bi-people',
    title: 'Agents',
    desc:  'Specialized sub-agents (engineer, architect, QA…). Each has a focused system prompt, tool set, and model selection.',
    color: '#8b5cf6',
  },
  {
    icon:  'bi-clock',
    title: 'Cron',
    desc:  'Scheduled tasks that run automatically at set intervals, even when the Copilot is idle.',
    color: '#f97316',
  },
  {
    icon:  'bi-cpu',
    title: 'Models',
    desc:  'Manage LLM, transcription, and image generation models. Drag to reorder priority.',
    color: '#10b981',
  },
  {
    icon:  'bi-plug',
    title: 'Providers',
    desc:  'Add API keys for LLM providers (Anthropic, OpenAI, OpenRouter, Ollama…).',
    color: '#06b6d4',
  },
  {
    icon:  'bi-shield-check',
    title: 'Security',
    desc:  'Define rules to auto-approve or auto-reject tool calls — skip repetitive confirmation prompts.',
    color: '#ef4444',
  },
];

export class HomePage extends InboxMixin(LightElement) {

  static get properties() {
    return {
      ...super.properties,
      _open:         { state: true },
      _models:       { state: true },
      _plugins:      { state: true },
      _debugMode:    { state: true },
      _debugLoading: { state: true },
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
    this._debugMode    = false;
    this._debugLoading = true;
    this._stats          = null;   // null = loading
    this._statsRange     = 'week';
    this._chartInstances = {};
    this._statsTimer     = null;
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'home';
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
      this._loadDebugMode(),
    ]);
  }

  async _loadDebugMode() {
    try {
      const res = await fetch('/api/dev/debug_mode');
      if (!res.ok) throw new Error();
      const data = await res.json();
      this._debugMode = data.enabled;
    } catch {
      // ignore, keep current value
    } finally {
      this._debugLoading = false;
    }
  }

  async _toggleDebugMode() {
    const next = !this._debugMode;
    this._debugMode = next;
    try {
      const res = await fetch('/api/dev/debug_mode', {
        method:  'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({ enabled: next }),
      });
      if (!res.ok) throw new Error();
      window.dispatchEvent(new CustomEvent('debug-mode-change', { detail: { enabled: next } }));
    } catch {
      this._debugMode = !next;
    }
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
    if (this._models === null)       return { cls: 'loading', dot: false, icon: null,                    text: 'Loading…' };
    if (this._models.length === 0)   return { cls: 'error',   dot: false, icon: 'bi-exclamation-circle-fill', text: 'No LLM models' };
    if (this._models.some(m => m.status === 'healthy'))  return { cls: 'online',  dot: true,  icon: null,                    text: 'Online & ready' };
    if (this._models.some(m => m.status === 'degraded')) return { cls: 'warn',    dot: true,  icon: 'bi-exclamation-triangle-fill', text: 'Degraded' };
    return { cls: 'error', dot: false, icon: 'bi-exclamation-circle-fill', text: 'All models offline' };
  }

  _nav(page) {
    const url = page === 'home' ? location.pathname : '#' + page;
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
    return { hour: '/ min', day: '/ hour', week: '/ day', month: '/ day' }[this._statsRange] ?? '/ day';
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
          lineDs(inp,   '#3b82f6', 'rgba(59,130,246,0)',  { fill: false, label: 'Input'  }),
          lineDs(out,   '#10b981', 'rgba(16,185,129,0)',  { fill: false, label: 'Output' }),
          lineDs(cache, '#f59e0b', 'rgba(245,158,11,0)',  { fill: false, label: 'Cached' }),
        ] : (() => {
          const nonCached = inp.map((v, i) => Math.max(0, v - (cache[i] ?? 0)));
          return [
            { label: 'Cached',     data: cache,     backgroundColor: '#f59e0b', stack: 'tok', borderSkipped: false },
            { label: 'Non-cached', data: nonCached, backgroundColor: '#3b82f6', stack: 'tok', borderSkipped: false },
            { label: 'Output',     data: out,        backgroundColor: '#10b981', stack: 'tok', borderRadius: 4, borderSkipped: false },
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
              return `Cache hit: ${pct}%`;
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
      return html`<div class="home-stats-loading"><i class="bi bi-hourglass-split"></i> Loading stats…</div>`;
    }

    const empty = this._stats.daily.length === 0 && this._stats.models.length === 0;
    if (empty) {
      return html`
        <div class="home-stats-empty">
          <i class="bi bi-bar-chart"></i>
          <span>No LLM requests in the selected range.</span>
        </div>
      `;
    }

    return html`
      <div class="home-stats-grid">
        <div class="home-stat-card">
          <div class="home-stat-card-title">Requests ${this._periodLabel}</div>
          <div class="home-stat-canvas-wrap"><canvas id="chart-requests"></canvas></div>
        </div>
        <div class="home-stat-card">
          <div class="home-stat-card-title">Tokens ${this._periodLabel}</div>
          <div class="home-stat-canvas-wrap"><canvas id="chart-tokens"></canvas></div>
        </div>
        <div class="home-stat-card">
          <div class="home-stat-card-title">Avg latency (ms)</div>
          <div class="home-stat-canvas-wrap"><canvas id="chart-latency"></canvas></div>
        </div>
        <div class="home-stat-card">
          <div class="home-stat-card-title">Models</div>
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

        <!-- ── Debug toggle ── -->
        <div class="home-debug-bar">
          <label class="home-debug-toggle" title="${this._debugMode ? 'Debug mode on' : 'Debug mode off'}">
            <i class="bi bi-bug-fill"></i>
            <span>Debug</span>
            <div class="form-check form-switch mb-0">
              <input class="form-check-input" type="checkbox"
                     .checked=${this._debugMode}
                     @change=${this._toggleDebugMode}
                     ?disabled=${this._debugLoading} />
            </div>
          </label>
        </div>

        <!-- ── Hero ── -->
        <div class="home-hero">
          <div class="home-hero-image">
            <img src="/assets/icons/icon-1024.png" alt="Skald" />
          </div>
          <div class="home-hero-text">
            <h1 class="home-hero-title">Skald</h1>
            <p class="home-hero-desc">Your AI command centre — research, code, plan, and orchestrate. All in one place.</p>
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
              <strong>No LLM models configured.</strong>
              Start by adding a provider (Anthropic, OpenAI, OpenRouter…), then add at least one model in the Models section.
            </div>
            <button class="btn btn-sm btn-danger" @click=${() => this._nav('providers')}>
              Add a provider
            </button>
          </div>
        ` : nothing}

        <!-- ── LLM Stats ── -->
        <div class="home-section-title">
          <i class="bi bi-bar-chart-fill"></i>
          <span>LLM Stats</span>
          <div class="home-stats-range ms-auto">
            ${[['hour','1h'],['day','24h'],['week','7d'],['month','30d']].map(([r, label]) => html`
              <button class="home-stats-range-btn ${this._statsRange === r ? 'active' : ''}"
                      @click=${() => this._setRange(r)}>${label}</button>
            `)}
          </div>
        </div>
        ${this._renderStats()}

        <!-- ── Pending inbox ── -->
        <div class="home-section-title">
          <i class="bi bi-inbox"></i>
          <span>Pending</span>
          ${inboxTotal > 0 ? html`<span class="badge bg-danger">${inboxTotal}</span>` : nothing}
          <button class="inbox-refresh-btn ms-auto" title="Refresh" @click=${() => this._loadInbox()}>
            <i class="bi bi-arrow-clockwise"></i>
          </button>
        </div>
        ${this._renderInboxSection()}

        <!-- ── Honcho tip ── -->
        ${!this._honchoActive ? html`
          <div class="home-tip">
            <div class="home-tip-icon"><i class="bi bi-lightbulb-fill"></i></div>
            <div class="home-tip-body">
              <strong>Enable Honcho</strong>
              <span>Persistent long-term memory — the agent learns your preferences over time. Ask the Copilot to enable it.</span>
            </div>
          </div>
        ` : nothing}

        <!-- ── Quick guide ── -->
        <div class="home-section-title">
          <i class="bi bi-map"></i>
          <span>Quick guide</span>
        </div>
        <div class="home-guide">
          ${GUIDE.map(s => html`
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
