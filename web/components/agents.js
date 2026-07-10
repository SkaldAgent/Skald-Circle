import { html }        from 'lit';
import { unsafeHTML }  from 'lit/directives/unsafe-html.js';
import { LightElement, renderMarkdown } from '../lib/base.js';

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
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'agents';
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open && this._agents.length === 0) this._loadList();
      if (!this._open) this._detail = null;
    });
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

  _strengthDot(strength, size = '0.62rem') {
    if (!strength) return html`<span style="opacity:0.3;font-size:${size}">—</span>`;
    return html`
      <span class="agent-strength-dot"
            style="background:${STRENGTH_COLORS[strength] ?? '#888'}"
            title=${STRENGTH_LABELS[strength] ?? strength}></span>
    `;
  }

  _scopePill(scope) {
    return html`<span class="agent-scope-pill">${scope}</span>`;
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
                  <span>${STRENGTH_LABELS[agent.strength] ?? agent.strength}</span>
                </span>
              ` : ''}
              ${agent.scope ? html`${this._scopePill(agent.scope)}` : ''}
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
    if (this._loading) return html`<div class="text-muted py-4 text-center">Loading…</div>`;
    if (this._error)   return html`<div class="alert alert-danger py-2" style="font-size:0.85rem">${this._error}</div>`;
    if (this._agents.length === 0) return html`<p class="text-muted">No agents found.</p>`;
    // Group by role: chat entry-points, dispatchable task executors, and
    // runtime-internal system agents (e.g. tic).
    const chat   = this._agents.filter(a => a.type === 'chat');
    const task   = this._agents.filter(a => a.type === 'task');
    const system = this._agents.filter(a => a.type === 'system');
    return html`
      ${this._renderSection('Chat', chat)}
      ${this._renderSection('Task Executors', task)}
      ${this._renderSection('System', system)}
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
          ${m.is_default ? html`<span class="badge bg-primary ms-1" style="font-size:0.6rem">default</span>` : ''}
        </td>
        <td class="text-muted agent-model-id">${m.model_id}</td>
        <td>
          ${(m.scope ?? []).map(s => this._scopePill(s))}
        </td>
      </tr>
    `;
  }

  _renderDetail() {
    if (this._loading && !this._detail) return html`<div class="text-muted py-4 text-center">Loading…</div>`;
    if (!this._detail) return '';

    const { meta, prompt, models } = this._detail;

    return html`
      <div class="agent-detail">
        <!-- Header -->
        <div class="agent-detail-header">
          <button class="btn btn-sm btn-link px-0" @click=${() => this._back()}>
            <i class="bi bi-arrow-left me-1"></i>Agents
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
          <!-- Meta -->
          <section class="agent-section">
            <h3 class="agent-section-title">Metadata</h3>
            <table class="agent-meta-table">
              <tbody>
                <tr><td class="agent-meta-key">ID</td><td><code>${meta.id}</code></td></tr>
                ${meta.strength ? html`
                  <tr><td class="agent-meta-key">Strength</td>
                    <td class="d-flex align-items-center gap-2">
                      ${this._strengthDot(meta.strength)}
                      ${STRENGTH_LABELS[meta.strength] ?? meta.strength}
                    </td>
                  </tr>
                ` : ''}
                ${meta.scope ? html`
                  <tr><td class="agent-meta-key">Scope</td><td>${this._scopePill(meta.scope)}</td></tr>
                ` : ''}
                ${meta.client ? html`
                  <tr><td class="agent-meta-key">Pinned model</td><td><code>${meta.client}</code></td></tr>
                ` : ''}
                ${meta.inject_memory?.length ? html`
                  <tr><td class="agent-meta-key">Memory files</td>
                    <td>${meta.inject_memory.map(f => html`<div style="font-size:0.8rem"><code>${f}</code></div>`)}</td>
                  </tr>
                ` : ''}
              </tbody>
            </table>
          </section>

          <!-- Model resolution order -->
          <section class="agent-section">
            <h3 class="agent-section-title">Model resolution order</h3>
            <p class="text-muted mb-2" style="font-size:0.8rem">
              Models sorted by how well they match this agent's requirements.
              The system uses the first available model from the top.
            </p>
            ${models.length === 0
              ? html`<p class="text-muted" style="font-size:0.85rem">No models configured.</p>`
              : html`
                <div class="table-responsive">
                  <table class="table table-sm agent-model-table mb-0">
                    <thead>
                      <tr>
                        <th>#</th>
                        <th>Strength</th>
                        <th>Name</th>
                        <th>Model ID</th>
                        <th>Scope</th>
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

          <!-- System prompt -->
          <section class="agent-section">
            <h3 class="agent-section-title">System prompt</h3>
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
              <h2 class="llm-page-title">Agents</h2>
            </div>

            <div class="agent-info-banner">
              <div class="agent-info-banner-icon"><i class="bi bi-info-circle-fill"></i></div>
              <div class="agent-info-banner-body">
                <p class="mb-1"><strong>Read-only view.</strong> Agents are defined by files in <code>agents/</code>
                — to add, remove, or modify an agent, edit the corresponding <code>AGENT.md</code> file in that
                directory.</p>
                <p class="mb-0">You can also ask <strong>Copilot</strong> (top bar) to create a new agent for you
                — just describe what it should do and it will set up all the files automatically.</p>
              </div>
            </div>

            ${this._renderList()}
          `
        }
      </div>
    `;
  }
}
