import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';

// Connector marketplace — blueprint §14/§15.
//
// Admin-only: browses the remote feed of vetted connectors and *installs* one into
// the local catalog. Installing is deliberately not activating — a global entry
// still needs the admin to enable it with a key, a per-user one still needs each
// user to activate it from the Connectors page. The feed only ever proposes; the
// trust anchor stays on this box.
//
// Page shell from the shared `um-*` styling; the card grid, chips and filter bar
// live in `css/connectors.css`. Colours come from the theme's own variables — no
// literal colour belongs in here.

const ADMIN_ID = 'admin';

async function jf(url, opts) {
  const res = await fetch(url, opts);
  if (!res.ok) throw new Error(await res.text() || `HTTP ${res.status}`);
  const ct = res.headers.get('content-type') || '';
  return ct.includes('application/json') ? res.json() : null;
}

export class MarketplacePage extends LightElement {

  static get properties() {
    return {
      _open:       { state: true },
      _me:         { state: true },
      _cards:      { state: true },
      _feedErr:    { state: true },   // feed unreachable — scoped, not page-level
      _error:      { state: true },
      _q:          { state: true },
      _scope:      { state: true },   // 'all' | 'per_user' | 'global'
      _source:     { state: true },   // 'all' | 'remote' | 'local_script'
      _installing: { state: true },
    };
  }

  constructor() {
    super();
    this._open = false;
    this._q = '';
    this._scope = 'all';
    this._source = 'all';
    this._reset();
  }

  _reset() {
    this._me = null;
    this._cards = null;
    this._feedErr = null;
    this._error = null;
    this._installing = null;
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'marketplace';
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) this._load();
    });
  }

  get _isAdmin() { return this._me?.role_id === ADMIN_ID; }

  async _load() {
    this._error = null;
    try {
      this._me = await jf('/api/auth/me');
      if (!this._isAdmin) return;
      await this._loadFeed(false);
    } catch (e) {
      this._error = e.message;
    }
  }

  async _loadFeed(refresh) {
    this._feedErr = null;
    if (refresh) this._cards = null;
    try {
      const res = await jf(`/api/mcp/marketplace${refresh ? '?refresh=true' : ''}`);
      this._cards = res.connectors ?? [];
    } catch (e) {
      this._cards = [];
      this._feedErr = e.message;
    }
  }

  async _install(card) {
    const warn = card.source === 'local_script'
      ? `\n\nThis puts code on this box:\n  • ${card.file_count} file(s), each verified against its SHA-256\n  • installed into ./connectors/${card.id}/`
      : '';
    if (!confirm(`Install "${card.name}" into the catalog?${warn}\n\nInstalling does not activate it.`)) return;
    this._installing = card.id;
    this._error = null;
    try {
      await jf('/api/mcp/marketplace/install', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ id: card.id }),
      });
      await this._loadFeed(false);
    } catch (e) {
      this._error = e.message;
    } finally {
      this._installing = null;
    }
  }

  // Client-side: the feed is small, and one payload keeps typing instant.
  get _filtered() {
    const q = this._q.trim().toLowerCase();
    return (this._cards ?? []).filter((c) => {
      if (this._scope !== 'all' && c.scope !== this._scope) return false;
      if (this._source !== 'all' && c.source !== this._source) return false;
      if (!q) return true;
      const hay = [c.name, c.id, c.user_description, ...(c.tags ?? []), ...(c.requires ?? [])]
        .filter(Boolean).join(' ').toLowerCase();
      return hay.includes(q);
    });
  }

  // The marketplace is a destination of the catalog's "Add connector" action, not a
  // place of its own — so it goes back where it came from.
  _goCatalog() {
    history.pushState({ page: 'catalog' }, '', '#catalog');
    window.dispatchEvent(new CustomEvent('llm-page-change', { detail: { page: 'catalog' } }));
  }

  render() {
    if (!this._open) return nothing;
    const loading = this._cards === null && !this._feedErr && !this._error;

    return html`
      <div class="um-page">
        <div class="um-header">
          <h2 class="um-title"><i class="bi bi-shop me-2"></i>Marketplace</h2>
          <div class="um-header-right">
            <button class="btn btn-sm btn-outline-primary" @click=${() => this._goCatalog()}>
              <i class="bi bi-arrow-left me-1"></i>Catalog
            </button>
            ${this._isAdmin ? html`
              <button class="um-btn-icon ms-1" title="Refetch the feed"
                @click=${() => this._loadFeed(true)}><i class="bi bi-arrow-clockwise"></i></button>
            ` : nothing}
          </div>
        </div>

        <div style="padding:0 1.25rem 1.5rem; overflow:auto">
          ${this._error ? html`
            <div class="alert alert-danger py-2 mt-3" style="font-size:.85rem">${this._error}</div>` : nothing}

          ${this._me && !this._isAdmin ? html`
            <div class="um-empty" style="padding:2rem">
              <i class="bi bi-shield-lock"></i>
              <p>The marketplace is managed by the admin.</p>
              <p style="font-size:.8rem;opacity:.7">
                Connectors the admin has installed appear on the
                <a href="#connectors" @click=${(e) => { e.preventDefault();
                  history.pushState({ page: 'connectors' }, '', '#connectors');
                  window.dispatchEvent(new CustomEvent('llm-page-change', { detail: { page: 'connectors' } })); }}>Connectors</a> page.
              </p>
            </div>
          ` : html`
            <div class="text-muted mt-3 mb-3" style="font-size:.8rem">
              Vetted connectors you can add to this box's catalog. Installing does not
              activate anything — it makes a connector <em>available</em>.
            </div>

            ${this._feedErr ? html`
              <div class="alert alert-warning py-2" style="font-size:.82rem">
                <i class="bi bi-wifi-off me-1"></i>Marketplace unreachable — ${this._feedErr}
              </div>` : nothing}

            ${this._renderFilters()}

            ${loading ? html`<div class="um-empty" style="padding:1rem"><i class="bi bi-hourglass-split"></i><p>Loading feed…</p></div>`
              : this._renderGrid()}
          `}
        </div>
      </div>`;
  }

  // A segmented control per axis rather than loose buttons: each row is one choice,
  // and the grouping says so.
  _segment(label, current, set, options) {
    return html`
      <div class="d-flex align-items-center gap-1">
        <span class="connector-segment-label">${label}</span>
        <div class="connector-segment">
          ${options.map(([text, value]) => html`
            <button class=${current === value ? 'active' : ''} @click=${() => set(value)}>${text}</button>`)}
        </div>
      </div>`;
  }

  _renderFilters() {
    return html`
      <div class="connector-filters">
        <div class="connector-search">
          <i class="bi bi-search"></i>
          <input class="form-control form-control-sm" placeholder="Search connectors…"
            .value=${this._q} @input=${(e) => { this._q = e.target.value; }} />
        </div>
        ${this._segment('Scope', this._scope, (v) => { this._scope = v; },
          [['All', 'all'], ['Global', 'global'], ['Per-user', 'per_user']])}
        ${this._segment('Type', this._source, (v) => { this._source = v; },
          [['All', 'all'], ['Remote', 'remote'], ['Local', 'local_script']])}
      </div>`;
  }

  _renderGrid() {
    const cards = this._filtered;
    const total = (this._cards ?? []).length;
    if (cards.length === 0) {
      return html`
        <div class="um-empty" style="padding:1rem"><i class="bi bi-search"></i>
          <p>${total === 0 ? 'The feed is empty.' : 'No connector matches these filters.'}</p></div>`;
    }
    return html`
      <div class="connector-grid">
        ${cards.map((c) => this._renderCard(c))}
      </div>`;
  }

  _renderCard(c) {
    const busy = this._installing === c.id;
    const isScript = c.source === 'local_script';
    // Keywords only. `mcp` is on everything, and scope/type already have their own
    // chips — repeating them as grey tags is noise.
    const tags = (c.tags ?? []).filter((t) => !['mcp', 'local', 'remote'].includes(t));

    return html`
      <div class="connector-card">
        <div class="connector-card-head">
          ${c.has_icon
            ? html`<img class="connector-card-icon" src=${`/api/mcp/marketplace/${c.id}/icon?size=sm`} alt="" />`
            : html`<div class="connector-card-icon connector-card-icon--empty"><i class="bi bi-plug"></i></div>`}
          <div class="connector-card-title">
            <div class="connector-card-name">${c.name}</div>
            <div class="connector-card-sub">${c.id}${c.version ? ` · v${c.version}` : ''}</div>
          </div>
          ${c.installed ? html`<span class="connector-chip connector-chip--ok">installed</span>` : nothing}
        </div>

        ${c.user_description ? html`<div class="connector-card-desc">${c.user_description}</div>` : nothing}

        <div class="connector-chips">
          <span class="connector-chip connector-chip--scope">
            <i class="bi ${c.scope === 'global' ? 'bi-globe' : 'bi-person'}"></i>
            ${c.scope === 'global' ? 'global' : 'per-user'}
          </span>
          <span class="connector-chip ${isScript ? 'connector-chip--script' : ''}">
            <i class="bi ${isScript ? 'bi-file-earmark-code' : 'bi-cloud'}"></i>
            ${isScript ? 'local script' : 'remote'}
          </span>
          ${c.auth_kind !== 'none' ? html`
            <span class="connector-chip"><i class="bi bi-key"></i>${c.auth_kind}</span>` : nothing}
          ${tags.map((t) => html`<span class="connector-chip">${t}</span>`)}
        </div>

        ${isScript ? html`
          <div class="connector-card-note">
            <i class="bi bi-shield-check"></i>${c.file_count} file${c.file_count === 1 ? '' : 's'}, SHA-256 verified on install
          </div>` : nothing}
        ${c.oauth_scopes?.length ? html`
          <details class="connector-card-scopes">
            <summary>Requests ${c.oauth_scopes.length} OAuth scope${c.oauth_scopes.length === 1 ? '' : 's'}</summary>
            ${c.oauth_scopes.map((s) => html`<code>${s}</code>`)}
          </details>` : nothing}

        <div class="connector-card-actions">
          <button class="btn btn-sm ${c.installed ? 'btn-outline-primary' : 'btn-primary'}"
            ?disabled=${busy} @click=${() => this._install(c)}>
            ${busy ? html`<i class="bi bi-hourglass-split me-1"></i>Installing…`
              : c.installed ? html`<i class="bi bi-arrow-repeat me-1"></i>Reinstall`
              : html`<i class="bi bi-download me-1"></i>Install`}
          </button>
          ${c.homepage ? html`
            <a class="btn btn-sm btn-outline-primary"
              href=${c.homepage} target="_blank" rel="noopener noreferrer" title="Homepage">
              <i class="bi bi-box-arrow-up-right"></i></a>` : nothing}
        </div>
      </div>`;
  }
}
