import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';
import { t }            from '../lib/i18n.js';
import {
  announceChange, authLabel, connectorIconUrl, jf, normalizeSchema, parseJson, seedEnv, statusOf,
} from './shared/connector-common.js';

// One connector's own page — `#connector?name=<catalog name>`.
//
// This replaces the activation dialog. A connector declares its own env/secret
// schema, so the form's height is the *connector's* choice, not the UI's: EMAIL asks
// for a dozen fields, and a fixed-size modal simply could not hold them — it grew
// taller than the viewport and the buttons went off-screen. A page scrolls.
//
// It is also the natural home for the other per-connector actions: the Test button
// and the global enable. Access grants are **not** here on purpose: they live only
// on the Users page, so "who has what" has a single surface.
//
// Deliberately not a `name` field: the list is one row per connector (§7 template),
// so the runtime name is the catalog name. The backend still defends against
// collisions; the UI just stops offering a way to cause them.

const ADMIN_ID = 'admin';
const PAGE_ID  = 'connector';

function nameFromHash() {
  const m = location.hash.match(/^#connector\?name=(.*)$/);
  if (!m) return null;
  try { return decodeURIComponent(m[1]); } catch { return null; }
}

export class ConnectorDetailPage extends LightElement {

  static get properties() {
    return {
      _open:      { state: true },
      _name:      { state: true },
      _me:        { state: true },
      _entry:     { state: true },   // catalog row (null for a global we cannot read)
      _act:       { state: true },   // my activation row, if any
      _glob:      { state: true },   // the global instance, if any
      _schema:    { state: true },
      _form:      { state: true },   // { api_key, env: {} }
      _test:      { state: true },   // null | 'running' | report
      _busy:      { state: true },
      _error:     { state: true },
      _noIcon:    { state: true },
      _oauth:     { state: true },   // in-flight OAuth login: { state, auth_url, code }
      _qr:        { state: true },   // in-flight QR/device login: { state, qr, message }
    };
  }

  constructor() {
    super();
    this._open = false;
    this._noIcon = false;
    this._reset();
  }

  _reset() {
    this._name   = null;
    this._me     = null;
    this._entry  = null;
    this._act    = null;
    this._glob   = null;
    this._schema = [];
    this._form   = { api_key: '', env: {} };
    this._test   = null;
    this._busy   = false;
    this._error  = null;
    this._oauth  = null;
    this._qr     = null;
    this._qrServerId = null;
    this._stopQrPoll();
  }

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === PAGE_ID;
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) this._loadFromHash();
      else this._stopQrPoll();   // never poll a connector's login off-screen
    });
    window.addEventListener('hashchange', () => {
      if (this._open) this._loadFromHash();
    });
  }

  disconnectedCallback() {
    window.removeEventListener('locale-changed', this.__onLocaleChanged);
    this._stopQrPoll();
    super.disconnectedCallback();
  }

  get _isAdmin()  { return this._me?.role_id === ADMIN_ID; }
  get _isGlobal() { return (this._entry?.scope ?? (this._glob ? 'global' : null)) === 'global'; }
  get _status()   {
    const s = statusOf({ _act: this._act, _glob: this._glob });
    // A QR/device connector at `pending` is waiting for its scan, not misconfigured.
    if (s === 'pending' && this._entry?.auth_kind === 'qr') return 'needs_login';
    return s;
  }

  async _loadFromHash() {
    const name = nameFromHash();
    if (!name) return;
    // A different connector must not inherit the previous one's typed secrets.
    if (name !== this._name) this._reset();
    this._name = name;
    await this._load();
  }

  async _load() {
    this._error = null;
    try {
      this._me = await jf('/api/auth/me');
      const [available, activated] = await Promise.all([
        jf('/api/mcp/available'),
        jf('/api/mcp/activated'),
      ]);
      const entry = (available?.catalog ?? []).find(e => e.name === this._name) ?? null;
      const glob  = (available?.globals ?? [])
        .find(g => (g.catalog_name ?? g.name) === this._name) ?? null;
      const act   = (activated ?? []).find(r => r.catalog_name === this._name) ?? null;

      if (!entry && !glob) {
        this._error = t('connectors.error.no_connector', { name: this._name });
        return;
      }
      this._entry = entry;
      this._glob  = glob;
      this._act   = act;

      const schema = normalizeSchema(parseJson(entry?.config_schema_json, []));
      this._schema = schema;
      // Keep whatever the user has already typed across a reload triggered by a save.
      this._form = { api_key: this._form.api_key || '', env: { ...seedEnv(schema), ...this._form.env } };
    } catch (e) {
      this._error = e.message;
    }
  }

  _back() {
    // Prefer real history so the browser's own Back stays consistent; fall back to
    // the list when this page was opened straight from a pasted URL.
    if (history.length > 1) { history.back(); return; }
    history.pushState({ page: 'connectors' }, '', '#connectors');
    window.dispatchEvent(new CustomEvent('llm-page-change', { detail: { page: 'connectors' } }));
  }

  _patchEnv(key, value) {
    this._form = { ...this._form, env: { ...this._form.env, [key]: value } };
  }

  /// The env map to send: empty fields are dropped so a blank box means "unset"
  /// rather than "set to empty string".
  get _envPayload() {
    const env = {};
    for (const [k, v] of Object.entries(this._form.env || {})) if (v !== '') env[k] = v;
    return Object.keys(env).length ? env : null;
  }

  // ── Actions ────────────────────────────────────────────────────────────────

  async _testCreds() {
    this._test = 'running';
    try {
      this._test = await jf('/api/mcp/test', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          catalog_name: this._name,
          api_key: this._form.api_key || null,
          env: this._envPayload,
        }),
      });
    } catch (e) {
      this._test = { ok: false, message: e.message };
    }
  }

  async _activate() {
    this._busy = true; this._error = null;
    try {
      const res = await jf('/api/mcp/activate', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          catalog_name: this._name,
          api_key: this._form.api_key || null,
          env: this._envPayload,
        }),
      });
      if (res?.auth_state === 'pending') {
        this._test  = res.verify ?? { ok: false, message: 'Verification failed.' };
        this._error = t('connectors.detail.test.error_saved');
      } else if (res?.error) {
        this._error = res.error;
      }
      announceChange();
      await this._load();
    } catch (e) { this._error = e.message; }
    finally { this._busy = false; }
  }

  async _deactivate() {
    if (!confirm(t('connectors.detail.confirm.deactivate', { name: this._entry?.friendly_name || this._name }))) return;
    this._busy = true;
    try {
      await jf(`/api/mcp/activated/${this._act.id}`, { method: 'DELETE' });
      this._act = null;
      this._test = null;
      announceChange();
      await this._load();
    } catch (e) { this._error = e.message; }
    finally { this._busy = false; }
  }

  // ── OAuth login (§15): activate → consent in a tab → paste code → complete ────

  async _startOauth() {
    this._busy = true; this._error = null;
    try {
      // The activation may not exist yet (first sign-in) — create the pending row,
      // then reuse it. A `pending` row from a previous attempt is signed in again.
      let serverId = this._act?.id;
      if (!serverId) {
        const res = await jf('/api/mcp/activate', {
          method: 'POST', headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ catalog_name: this._name }),
        });
        if (res?.error) { this._error = res.error; return; }
        serverId = res.id;
      }
      const start = await jf('/api/mcp/oauth/start', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ server_id: serverId }),
      });
      this._oauth = { state: start.state, auth_url: start.auth_url, code: '' };
      window.open(start.auth_url, '_blank', 'noopener');
      announceChange();
      await this._load();
    } catch (e) { this._error = e.message; }
    finally { this._busy = false; }
  }

  async _completeOauth() {
    this._busy = true; this._error = null;
    try {
      const res = await jf('/api/mcp/oauth/complete', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ state: this._oauth.state, code: this._oauth.code.trim() }),
      });
      if (res?.error) { this._error = res.error; }
      else { this._oauth = null; }
      announceChange();
      await this._load();
    } catch (e) { this._error = e.message; }
    finally { this._busy = false; }
  }

  // ── QR / device login (§15): activate → server emits a QR → scan → poll ready ──
  // Unlike OAuth there is no code to paste: the connector's server must run to
  // produce the QR, so activation starts it and we poll `login_status` until the
  // phone scan flips it to `ready`.

  async _startQrLogin() {
    this._busy = true; this._error = null;
    try {
      // First sign-in creates the pending row (which installs deps + starts the
      // server — this can take a while on a cold container). Reuse it thereafter.
      let serverId = this._act?.id;
      if (!serverId) {
        const res = await jf('/api/mcp/activate', {
          method: 'POST', headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ catalog_name: this._name }),
        });
        if (res?.error) { this._error = res.error; return; }
        serverId = res.id;
      }
      this._qrServerId = serverId;
      await this._pollQr();      // fetch the first QR immediately
      this._startQrPoll();       // then keep it fresh
      await this._load();
    } catch (e) { this._error = e.message; }
    finally { this._busy = false; }
  }

  _startQrPoll() {
    this._stopQrPoll();
    // The QR rotates every ~20 s and the scan can land any moment: poll briskly.
    this.__qrTimer = setInterval(() => this._pollQr(), 2500);
  }

  _stopQrPoll() {
    if (this.__qrTimer) { clearInterval(this.__qrTimer); this.__qrTimer = null; }
  }

  async _pollQr() {
    if (!this._qrServerId) return;
    try {
      const res = await jf('/api/mcp/login/status', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ server_id: this._qrServerId }),
      });
      this._qr = res;
      if (res?.state === 'ready') {
        this._stopQrPoll();
        await this._load();      // pick up the flipped auth_state
      }
    } catch (_) { /* transient (server still connecting) — keep polling */ }
  }

  async _resetQrLogin() {
    const id = this._qrServerId || this._act?.id;
    if (!id) return;
    this._busy = true; this._error = null;
    try {
      await jf('/api/mcp/login/reset', {
        method: 'POST', headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ server_id: id }),
      });
      this._qr = null;
      this._qrServerId = id;
      await this._pollQr();
      this._startQrPoll();
      await this._load();
    } catch (e) { this._error = e.message; }
    finally { this._busy = false; }
  }

  async _enableGlobal() {
    this._busy = true; this._error = null;
    try {
      const res = await jf('/api/mcp/global', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          catalog_name: this._name,
          api_key: this._form.api_key || null,
          env: this._envPayload,
        }),
      });
      if (res?.verify && !res.verify.ok && !res.verify.skipped) {
        this._test  = res.verify;
        this._error = t('connectors.detail.test.error_verify');
      } else if (res?.error) {
        this._error = res.error;
      }
      announceChange();
      await this._load();
    } catch (e) { this._error = e.message; }
    finally { this._busy = false; }
  }

  async _disableGlobal() {
    if (!confirm(t('connectors.detail.confirm.disable_global', { name: this._glob.friendly_name || this._name }))) return;
    this._busy = true;
    try {
      await jf(`/api/mcp/global/${this._glob.id}`, { method: 'DELETE' });
      this._glob = null;
      announceChange();
      await this._load();
    } catch (e) { this._error = e.message; }
    finally { this._busy = false; }
  }

  // ── Render ─────────────────────────────────────────────────────────────────

  render() {
    if (!this._open) return nothing;
    if (this._error && !this._entry && !this._glob) {
      return html`
        <div class="um-page">
          ${this._renderHeader()}
          <div class="alert alert-danger py-2 mx-4" style="font-size:.85rem">${this._error}</div>
        </div>`;
    }
    if (!this._entry && !this._glob) {
      return html`<div class="um-page">${this._renderHeader()}
        <div class="um-empty"><i class="bi bi-hourglass-split"></i> ${t('connectors.loading')}</div></div>`;
    }

    return html`
      <div class="um-page">
        ${this._renderHeader()}
        <div style="padding:0 1.25rem 2rem; overflow:auto">
          ${this._error ? html`
            <div class="alert alert-danger py-2 mb-3" style="font-size:.85rem">${this._error}</div>` : nothing}
          ${this._renderSummary()}
          ${this._renderConfig()}
        </div>
      </div>`;
  }

  _renderHeader() {
    const title = this._entry?.friendly_name || this._glob?.friendly_name || this._name || 'Connector';
    return html`
      <div class="um-header">
        <div class="d-flex align-items-center gap-2" style="min-width:0">
          <button class="btn btn-sm btn-outline-secondary" title=${t('connectors.detail.back')} @click=${() => this._back()}>
            <i class="bi bi-arrow-left"></i>
          </button>
          <h2 class="um-title" style="min-width:0;overflow:hidden;text-overflow:ellipsis">${title}</h2>
        </div>
      </div>`;
  }

  _renderSummary() {
    const e        = this._entry;
    const isScript = e?.source === 'local_script';
    const status   = this._status;
    const desc     = e?.description || this._glob?.description;
    const _statusText = (s) => ({ active: t('connectors.detail.status.active'), pending: t('connectors.detail.status.needs_fix'), needs_login: t('connectors.detail.status.needs_signin') })[s] ?? s;

    return html`
      <div class="connector-card" style="margin-top:1rem">
        <div class="connector-card-head">
          ${!this._noIcon
            ? html`<img class="connector-card-icon" style="width:44px;height:44px"
                     src=${connectorIconUrl(this._name, 'lg')} alt=""
                     @error=${() => { this._noIcon = true; }} />`
            : html`<div class="connector-card-icon connector-card-icon--empty" style="width:44px;height:44px">
                     <i class="bi bi-plug"></i></div>`}
          <div class="connector-card-title">
            <div class="connector-card-name" style="font-size:1rem">
              ${e?.friendly_name || this._glob?.friendly_name || this._name}
            </div>
            <div class="connector-card-sub">${this._name}</div>
          </div>
        </div>
        ${desc ? html`<div class="connector-card-desc" style="-webkit-line-clamp:initial">${desc}</div>` : nothing}
        <div class="connector-chips">
          <span class="connector-chip">
            <i class="bi ${this._isGlobal ? 'bi-globe' : 'bi-person'}"></i>${this._isGlobal ? t('connectors.detail.detail_scope_global') : t('connectors.chip.per_user')}
          </span>
          ${isScript ? html`
            <span class="connector-chip connector-chip--script">
              <i class="bi bi-file-earmark-code"></i>${t('connectors.detail.scope_local')}
            </span>` : nothing}
          ${e?.auth_kind && e.auth_kind !== 'none' ? html`
            <span class="connector-chip"><i class="bi bi-key"></i>${authLabel(e.auth_kind)}</span>` : nothing}
          ${status === 'active' ? html`
            <span class="connector-chip connector-chip--ok"><i class="bi bi-check-circle"></i>${t('connectors.detail.status.active')}</span>` : nothing}
          ${status === 'pending' ? html`
            <span class="connector-chip connector-chip--script"><i class="bi bi-exclamation-triangle"></i>${t('connectors.detail.status.needs_fix')}</span>` : nothing}
          ${status === 'needs_login' ? html`
            <span class="connector-chip connector-chip--script"><i class="bi bi-box-arrow-in-right"></i>${t('connectors.detail.status.needs_signin')}</span>` : nothing}
        </div>
        ${this._isGlobal ? html`
          <div class="connector-card-note">
            <i class="bi bi-info-circle"></i>${t('connectors.detail.global_note')}
          </div>` : nothing}
      </div>`;
  }

  _renderConfig() {
    const e = this._entry;

    // A granted global we have no catalog row for: nothing here is ours to configure.
    if (!e) {
      return html`
        <div style="margin-top:1.5rem">
          <div class="um-empty" style="padding:1rem"><i class="bi bi-check2-circle"></i>
            <p>${t('connectors.detail.managed.title')}</p>
            <p style="font-size:.8rem;opacity:.7">${t('connectors.detail.managed.desc')}</p>
          </div>
        </div>`;
    }

    const active    = this._isGlobal ? !!this._glob : !!this._act;
    const canManage = this._isGlobal ? this._isAdmin : true;
    const hasVerify = !!e.verify_command;
    const oauth     = e.auth_kind === 'oauth';
    // An api_key connector that declares its key as a described `env[]` field (secret)
    // collects it there — the generic, label-less "API key" box would be a duplicate
    // asking for the same value. Fall back to the generic box only when the schema
    // names no secret of its own (a bare `requires:[API_KEY]` connector).
    const schemaHasSecret = this._schema.some(f => f.secret);

    if (this._isGlobal && !this._isAdmin) return nothing;

    // OAuth is a per-user, interactive flow — a browser consent, not a form of
    // typed credentials — so it gets its own panel instead of the api_key/env body.
    if (oauth && !this._isGlobal) {
      return html`
        <div style="margin-top:1.5rem">
          <div class="um-header" style="padding:0 0 .5rem">
            <h3 class="um-title" style="font-size:1rem"><i class="bi bi-key me-2"></i>${t('connectors.detail.oauth.title')}</h3>
          </div>
          ${this._renderOauth()}
        </div>`;
    }

    // QR / device login (WhatsApp): the server produces a QR the user scans with
    // their phone — its own panel, like OAuth.
    if (e.auth_kind === 'qr' && !this._isGlobal) {
      return html`
        <div style="margin-top:1.5rem">
          <div class="um-header" style="padding:0 0 .5rem">
            <h3 class="um-title" style="font-size:1rem"><i class="bi bi-qr-code me-2"></i>${t('connectors.detail.qr.title')}</h3>
          </div>
          ${this._renderQr()}
        </div>`;
    }

    return html`
      <div style="margin-top:1.5rem">
        <div class="um-header" style="padding:0 0 .5rem">
          <h3 class="um-title" style="font-size:1rem">
            <i class="bi bi-sliders me-2"></i>${active ? t('connectors.detail.config.title_active') : t('connectors.detail.config.title_setup')}
          </h3>
        </div>

        ${active ? html`
          <div class="text-muted mb-3" style="font-size:.78rem">
            ${this._isGlobal
              ? t('connectors.detail.config.already_global')
              : t('connectors.detail.config.already_user')}
          </div>` : nothing}

        ${e.auth_kind === 'api_key' && !schemaHasSecret ? html`
          <div class="mb-3">
            <label class="form-label">${t('connectors.detail.config.api_key')}<span class="text-danger">*</span></label>
            <input class="form-control" type="password" .value=${this._form.api_key}
              @input=${(ev) => { this._form = { ...this._form, api_key: ev.target.value }; }} />
          </div>` : nothing}

        ${this._renderEnvFields()}
        ${this._renderVerifyBox()}

        <div class="d-flex gap-2 flex-wrap" style="margin-top:.5rem">
          ${hasVerify && canManage ? html`
            <button class="btn btn-sm btn-outline-secondary" ?disabled=${this._test === 'running' || this._busy}
              @click=${() => this._testCreds()}>
              <i class="bi bi-${this._test === 'running' ? 'arrow-repeat' : 'check2-gear'} me-1"></i>
              ${this._test === 'running' ? t('connectors.detail.config.btn_testing') : t('connectors.detail.config.btn_test')}
            </button>` : nothing}

          ${this._isGlobal
            ? html`
              <button class="btn btn-sm btn-primary" ?disabled=${this._busy} @click=${() => this._enableGlobal()}>
                <i class="bi bi-globe me-1"></i>${this._glob ? t('connectors.detail.config.btn_save_restart') : t('connectors.detail.config.btn_enable_global')}
              </button>
              ${this._glob ? html`
                <button class="btn btn-sm btn-outline-danger" ?disabled=${this._busy} @click=${() => this._disableGlobal()}>
                  <i class="bi bi-trash me-1"></i>${t('connectors.detail.config.btn_disable')}
                </button>` : nothing}`
            : html`
              <button class="btn btn-sm btn-primary" ?disabled=${this._busy} @click=${() => this._activate()}>
                <i class="bi bi-plug me-1"></i>${this._act ? t('connectors.detail.config.btn_save_restart') : t('connectors.detail.config.btn_activate')}
              </button>
              ${this._act ? html`
                <button class="btn btn-sm btn-outline-danger" ?disabled=${this._busy} @click=${() => this._deactivate()}>
                  <i class="bi bi-trash me-1"></i>${t('connectors.detail.config.btn_deactivate')}
                </button>` : nothing}`}
        </div>
      </div>`;
  }

  _renderOauth() {
    const provider = this._entry?.oauth_provider || 'provider';
    const label    = provider.charAt(0).toUpperCase() + provider.slice(1);
    const active   = this._act && this._act.auth_state === 'ready';
    const pending  = this._act && this._act.auth_state === 'pending';
    const scopes   = parseJson(this._entry?.oauth_scopes_json, []);

    return html`
      <div class="text-muted mb-3" style="font-size:.78rem">${t('connectors.detail.oauth.desc', { provider: label })}</div>

      ${scopes.length ? html`
        <div class="mb-3" style="font-size:.72rem">
          <div class="text-muted mb-1">${t('connectors.detail.oauth.scopes')}</div>
          <ul class="mb-0 ps-3">${scopes.map(s => html`<li><code style="font-size:.68rem">${s}</code></li>`)}</ul>
        </div>` : nothing}

      ${active ? html`
        <div class="alert alert-success py-2 mb-3" style="font-size:.82rem">
          <i class="bi bi-check-circle-fill me-1"></i>${t('connectors.detail.oauth.signed_in')}
        </div>` : nothing}

      ${!this._oauth ? html`
        <div class="d-flex gap-2 flex-wrap">
          <button class="btn btn-sm btn-primary" ?disabled=${this._busy} @click=${() => this._startOauth()}>
            <i class="bi bi-box-arrow-in-right me-1"></i>
            ${active ? t('connectors.detail.oauth.btn_signin_again') : (pending ? t('connectors.detail.oauth.btn_finish') : t('connectors.detail.oauth.btn_signin', { provider: label }))}
          </button>
          ${this._act ? html`
            <button class="btn btn-sm btn-outline-danger" ?disabled=${this._busy} @click=${() => this._deactivate()}>
              <i class="bi bi-trash me-1"></i>${t('connectors.detail.oauth.deactivate')}
            </button>` : nothing}
        </div>`
      : html`
        <div class="connector-card" style="margin-top:.25rem">
          <div class="mb-2" style="font-size:.8rem">
            <i class="bi bi-1-circle me-1"></i>${t('connectors.detail.oauth.step1', { provider: label })}
            <div class="mt-1"><a href=${this._oauth.auth_url} target="_blank" rel="noopener">${t('connectors.detail.oauth.step1_link')}</a></div>
          </div>
          <div class="mb-2" style="font-size:.8rem">
            <i class="bi bi-2-circle me-1"></i>${t('connectors.detail.oauth.step2')}
          </div>
          <input class="form-control font-monospace mb-2" placeholder="4/0A…"
            .value=${this._oauth.code}
            @input=${(ev) => { this._oauth = { ...this._oauth, code: ev.target.value }; }} />
          <div class="d-flex gap-2">
            <button class="btn btn-sm btn-primary" ?disabled=${this._busy || !this._oauth.code.trim()}
              @click=${() => this._completeOauth()}>
              <i class="bi bi-check-lg me-1"></i>${t('connectors.detail.oauth.btn_complete')}
            </button>
            <button class="btn btn-sm btn-outline-secondary" ?disabled=${this._busy}
              @click=${() => { this._oauth = null; }}>${t('connectors.detail.oauth.cancel')}</button>
          </div>
        </div>`}
    `;
  }

  _renderQr() {
    const active  = this._act && this._act.auth_state === 'ready';
    const q       = this._qr;
    const st      = q?.state;
    const polling = !!this.__qrTimer;

    return html`
      <div class="text-muted mb-3" style="font-size:.78rem">${t('connectors.detail.qr.desc')}</div>

      ${active && st !== 'need_scan' && st !== 'logged_out' ? html`
        <div class="alert alert-success py-2 mb-3" style="font-size:.82rem">
          <i class="bi bi-check-circle-fill me-1"></i>${t('connectors.detail.qr.connected')}
        </div>` : nothing}

      ${st === 'need_scan' && q?.qr ? html`
        <div class="connector-card" style="text-align:center; margin-bottom:.75rem">
          <div class="mb-2" style="font-size:.82rem">${t('connectors.detail.qr.scan')}</div>
          <img src=${q.qr} alt="WhatsApp QR"
            style="width:280px; max-width:100%; height:auto; border-radius:8px; background:#fff; padding:10px" />
          <div class="text-muted mt-2" style="font-size:.72rem">${t('connectors.detail.qr.hint')}</div>
        </div>` : nothing}

      ${polling && st && st !== 'ready' && st !== 'need_scan' ? html`
        <div class="d-flex align-items-center gap-2 mb-2 text-muted" style="font-size:.8rem">
          <i class="bi bi-arrow-repeat"></i>${q?.message || t('connectors.detail.qr.connecting')}
        </div>` : nothing}

      <div class="d-flex gap-2 flex-wrap" style="margin-top:.5rem">
        ${!active && !polling ? html`
          <button class="btn btn-sm btn-primary" ?disabled=${this._busy} @click=${() => this._startQrLogin()}>
            <i class="bi bi-qr-code me-1"></i>${this._busy ? t('connectors.detail.qr.btn_starting') : t('connectors.detail.qr.btn_start')}
          </button>` : nothing}
        ${active || polling ? html`
          <button class="btn btn-sm btn-outline-secondary" ?disabled=${this._busy} @click=${() => this._resetQrLogin()}>
            <i class="bi bi-arrow-repeat me-1"></i>${t('connectors.detail.qr.btn_relink')}
          </button>` : nothing}
        ${this._act ? html`
          <button class="btn btn-sm btn-outline-danger" ?disabled=${this._busy} @click=${() => this._deactivate()}>
            <i class="bi bi-trash me-1"></i>${t('connectors.detail.oauth.deactivate')}
          </button>` : nothing}
      </div>`;
  }

  _renderEnvFields() {
    if (!this._schema.length) return nothing;
    return this._schema.map(f => html`
      <div class="mb-3">
        <label class="form-label">
          ${f.label || f.name}
          ${f.required ? html`<span class="text-danger">*</span>` : nothing}
          ${f.secret ? html` <span class="badge bg-warning text-dark" style="font-size:.6rem">secret</span>` : nothing}
        </label>
        <input
          class="form-control ${f.secret ? '' : 'font-monospace'}"
          type=${f.secret ? 'password' : 'text'}
          placeholder=${f.example || ''}
          .value=${this._form.env[f.name] ?? ''}
          @input=${(ev) => this._patchEnv(f.name, ev.target.value)} />
        ${f.description ? html`<div class="form-text" style="font-size:.72rem">${f.description}</div>` : nothing}
      </div>`);
  }

  _renderVerifyBox() {
    const result = this._test;
    if (result === null) return nothing;
    if (result === 'running') {
      return html`<div class="alert alert-secondary py-2 mb-3" style="font-size:.82rem">
        <i class="bi bi-arrow-repeat me-1"></i>${t('connectors.detail.test.running')}</div>`;
    }
    if (result.skipped) {
      return html`<div class="alert alert-secondary py-2 mb-3" style="font-size:.82rem">
        <i class="bi bi-info-circle me-1"></i>${result.message || t('connectors.detail.test.skipped')}</div>`;
    }
    return html`
      <div class="alert alert-${result.ok ? 'success' : 'danger'} py-2 mb-3" style="font-size:.82rem">
        <i class="bi ${result.ok ? 'bi-check-circle-fill' : 'bi-x-circle-fill'} me-1"></i>
        <strong>${result.ok ? t('connectors.detail.test.ok_label') : t('connectors.detail.test.fail_label')}</strong> — ${result.message}
        ${t.details ? html`
          <pre class="mb-0 mt-1 p-2 rounded bg-dark text-light"
            style="font-size:.7rem;white-space:pre-wrap">${JSON.stringify(t.details, null, 2)}</pre>` : nothing}
      </div>`;
  }
}
