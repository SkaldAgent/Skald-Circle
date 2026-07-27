// Mobile-connector "Mobile App" console (page_id `app`) — the plugin's single
// page: relay connection status, the device list, the pairing dialog, and —
// for admins — the settings dialog (the plugin's config lives here, not in the
// generic plugin-detail form; see `Plugin::config_in_detail_page`).
//
// Self-scoped per caller: an admin sees every device and may reassign/revoke
// any of them; anyone else sees only their own devices, can pair a new one
// (it auto-binds to them) and revoke their own. Default-exports the element
// class; the host registers it.
import { html, nothing } from 'lit';
import { MobileBase, jf, ago, deviceLabel, t } from './common.js';

const P = 'plugin.mobile-connector';

// Relay presets offered in the settings dialog. The official relay is not in
// service yet — shown disabled (the value is still recognised if configured
// by hand). A "custom" choice free-forms the wss:// URL.
const RELAY_OFFICIAL = 'wss://relay.skaldagent.net/v1/ws';
const RELAY_TEST     = 'wss://relay-test.skaldagent.net/v1/ws';

export default class MobileAppPage extends MobileBase {
  static get properties() {
    return {
      _status:  { state: true },   // { running, connected, relay_url, last_error } | null
      _devices: { state: true },   // [] | null (loading)
      _isAdmin: { state: true },
      _users:   { state: true },   // admin: [{id, username, display_name}]
      _pick:    { state: true },   // admin: { [pubkey]: user_id } reassign selections
      _error:   { state: true },
      _pair:    { state: true },   // dialog state | null
      _cfg:     { state: true },   // dialog state | null
    };
  }

  constructor() {
    super();
    this._status = null;
    this._devices = null;
    this._isAdmin = false;
    this._users = [];
    this._pick = {};
    this._error = null;
    this._pair = null;
    this._cfg = null;
    this._poll = null;
    this._pairPoll = null;
    this._pairTimer = null;
    this._knownPubkeys = new Set();
  }

  connectedCallback() {
    super.connectedCallback();
    this._init();
    this._poll = setInterval(() => this._load(true), 5000);
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    if (this._poll) { clearInterval(this._poll); this._poll = null; }
    this._stopPairWatch();
  }

  async _init() {
    try {
      const me = await jf('/api/auth/me');
      this._isAdmin = me?.role_id === 'admin';
    } catch { this._isAdmin = false; }
    await this._load();
  }

  async _load(quiet = false) {
    if (!quiet) this._error = null;
    try {
      this._status = await jf(`${this.api}/status`);
    } catch (e) {
      if (!quiet) this._error = e.message;
      this._status = { running: false, connected: false, relay_url: null, last_error: null };
    }
    if (!this._status.running) {
      this._devices = [];
      return;
    }
    try {
      const d = await jf(`${this.api}/devices`);
      this._devices = d.devices || [];
      if (this._isAdmin && !this._users.length) {
        try { this._users = await jf('/api/users'); } catch { /* the reassign dropdown stays empty */ }
      }
      this._detectPairing();
    } catch (e) {
      if (!quiet) this._error = e.message;
      if (this._devices === null) this._devices = [];
    }
  }

  // ── Pairing dialog ─────────────────────────────────────────────────────────

  _detectPairing() {
    // While the dialog is open, a pubkey we have never seen means the phone
    // just scanned the QR — switch the dialog to its success state.
    if (!this._pair || !this._pair.session || this._pair.paired) {
      this._knownPubkeys = new Set((this._devices || []).map(d => d.pubkey));
      return;
    }
    const fresh = (this._devices || []).find(d => !this._knownPubkeys.has(d.pubkey));
    if (fresh) {
      this._pair = { ...this._pair, paired: true };
      this._stopPairWatch();
    }
  }

  _startPairWatch() {
    this._stopPairWatch();
    this._pairPoll = setInterval(() => this._load(true), 2000);
    const tick = () => {
      if (!this._pair?.session) return this._stopPairWatch();
      const remain = Math.max(0, Math.round((this._pair.session.expires_at - Date.now()) / 1000));
      this._pair = { ...this._pair, remain };
      if (remain <= 0 && this._pairTimer) { clearInterval(this._pairTimer); this._pairTimer = null; }
    };
    tick();
    this._pairTimer = setInterval(tick, 1000);
  }

  _stopPairWatch() {
    if (this._pairPoll) { clearInterval(this._pairPoll); this._pairPoll = null; }
    if (this._pairTimer) { clearInterval(this._pairTimer); this._pairTimer = null; }
  }

  async _openPairing() {
    this._pair = { session: null, remain: 0, busy: true, error: null, paired: false };
    this._knownPubkeys = new Set((this._devices || []).map(d => d.pubkey));
    try {
      const session = await jf(`${this.api}/pairing`, { method: 'POST', body: JSON.stringify({}) });
      this._pair = { ...this._pair, session, busy: false };
      this._startPairWatch();
    } catch (e) {
      this._pair = { ...this._pair, busy: false, error: e.message };
    }
  }

  async _closePairing() {
    const had = this._pair?.session && !this._pair.paired;
    this._stopPairWatch();
    this._pair = null;
    // Best-effort close of the window we opened (a consumed/expired one is
    // already gone server-side; a paired one belongs to the new device).
    if (had) { try { await jf(`${this.api}/pairing`, { method: 'DELETE' }); } catch { /* ignore */ } }
  }

  // ── Device actions ─────────────────────────────────────────────────────────

  _userName(id) {
    const u = this._users.find(x => x.id === id);
    return u ? (u.display_name || u.username) : id;
  }

  async _bind(pubkey) {
    const user_id = this._pick[pubkey];
    if (!user_id) return;
    try {
      await jf(`${this.api}/devices/bind`, { method: 'POST', body: JSON.stringify({ pubkey, user_id }) });
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  async _revoke(pubkey) {
    if (!confirm(t(`${P}.devices.revoke_confirm`))) return;
    try {
      await jf(`${this.api}/devices/revoke`, { method: 'POST', body: JSON.stringify({ pubkey }) });
      await this._load();
    } catch (e) { this._error = e.message; }
  }

  // ── Settings dialog (admin) ────────────────────────────────────────────────

  async _openConfig() {
    this._cfg = { loading: true, error: null, ok: false, draft: null, relayChoice: 'test', customUrl: '', enabled: true };
    try {
      const all = await jf('/api/plugins');
      const p = (all ?? []).find(x => x.id === 'mobile-connector');
      if (!p) throw new Error(t(`${P}.cfg.not_found`));
      const c = p.config || {};
      const url = c.relay_url || '';
      const relayChoice = url === RELAY_OFFICIAL ? 'official' : (url === RELAY_TEST || !url) ? 'test' : 'custom';
      this._cfg = {
        ...this._cfg,
        loading: false,
        enabled: !!p.enabled,
        relayChoice,
        customUrl: relayChoice === 'custom' ? url : '',
        draft: {
          relay_url: url,
          pairing_ttl: c.pairing_ttl ?? 300,
          require_device_confirmation: c.require_device_confirmation !== false,
          notify_delay_secs: c.notify_delay_secs ?? 20,
        },
      };
    } catch (e) {
      this._cfg = { ...this._cfg, loading: false, error: e.message };
    }
  }

  _patchCfg(key, value) {
    this._cfg = { ...this._cfg, draft: { ...this._cfg.draft, [key]: value }, ok: false };
  }

  async _saveConfig() {
    const { draft, relayChoice, customUrl, enabled } = this._cfg;
    const relay_url = relayChoice === 'custom' ? (customUrl || '').trim()
      : relayChoice === 'official' ? RELAY_OFFICIAL : RELAY_TEST;
    if (relayChoice === 'custom' && !/^wss?:\/\/.+/.test(relay_url)) {
      this._cfg = { ...this._cfg, error: t(`${P}.cfg.bad_url`), ok: false };
      return;
    }
    this._cfg = { ...this._cfg, busy: true, error: null, ok: false };
    try {
      await jf('/api/plugins/mobile-connector', {
        method: 'PUT',
        body: JSON.stringify({ enabled, config: { ...draft, relay_url } }),
      });
      this._cfg = { ...this._cfg, busy: false, ok: true, draft: { ...draft, relay_url } };
      // The plugin reloads on save; the status poll picks up the reconnection.
      setTimeout(() => { if (this._cfg?.ok) this._cfg = null; this._load(true); }, 900);
    } catch (e) {
      this._cfg = { ...this._cfg, busy: false, error: e.message };
    }
  }

  // ── Render ─────────────────────────────────────────────────────────────────

  render() {
    return html`
      <div class="um-page">
        <div class="um-header d-flex justify-content-between align-items-center" style="flex-wrap:wrap;gap:.5rem">
          <h2 class="um-title"><i class="bi bi-phone me-2"></i>${t(`${P}.app.title`)}</h2>
          <div class="d-inline-flex gap-2 align-items-center">
            ${this._renderStatusPill()}
            <button class="btn btn-sm btn-primary" @click=${() => this._openPairing()}
                    ?disabled=${!this._status?.connected}>
              <i class="bi bi-qr-code-scan me-1"></i>${t(`${P}.app.pair_new`)}
            </button>
            ${this._isAdmin ? html`
              <button class="btn btn-sm btn-outline-secondary" title=${t(`${P}.cfg.open`)} @click=${() => this._openConfig()}>
                <i class="bi bi-gear"></i>
              </button>` : nothing}
          </div>
        </div>
        <div style="padding:0 1.25rem 1.5rem; max-width:860px">
          ${this._renderStatusAlerts()}
          ${this._error ? html`<div class="alert alert-danger py-2" style="font-size:.85rem">${this._error}</div>` : nothing}
          ${this._renderDevices()}
        </div>
        ${this._renderPairDialog()}
        ${this._renderConfigDialog()}
      </div>`;
  }

  _renderStatusPill() {
    const s = this._status;
    const [cls, icon, key] = !s ? ['text-bg-secondary', 'bi-hourglass-split', 'loading']
      : !s.running ? ['text-bg-secondary', 'bi-pause-circle', 'off']
      : s.connected ? ['text-bg-success', 'bi-check-circle', 'connected']
      : ['text-bg-warning', 'bi-arrow-repeat', 'connecting'];
    return html`
      <span class="badge ${cls} d-inline-flex align-items-center gap-1" style="font-size:.72rem">
        <i class="bi ${icon}"></i>${t(`${P}.status.${key}`)}
      </span>`;
  }

  _renderStatusAlerts() {
    const s = this._status;
    if (!s) return nothing;
    if (!s.running) {
      return html`
        <div class="alert alert-secondary py-2 d-flex align-items-start gap-2" style="font-size:.85rem">
          <i class="bi bi-info-circle mt-1"></i>
          <div>${t(this._isAdmin ? `${P}.status.off_hint_admin` : `${P}.status.off_hint`)}</div>
        </div>`;
    }
    if (!s.connected) {
      return html`
        <div class="alert alert-warning py-2" style="font-size:.85rem">
          <div class="d-flex align-items-start gap-2">
            <i class="bi bi-exclamation-triangle mt-1"></i>
            <div>
              ${t(`${P}.status.connecting_hint`)}
              ${s.last_error ? html`
                <div class="mt-1" style="font-family:var(--font-mono,monospace);font-size:.75rem;word-break:break-all">
                  ${t(`${P}.status.last_error`)}: ${s.last_error}
                </div>` : nothing}
            </div>
          </div>
        </div>`;
    }
    return nothing;
  }

  _renderDevices() {
    if (this._devices === null) {
      return html`<div class="um-empty"><i class="bi bi-hourglass-split"></i> ${t(`${P}.devices.loading`)}</div>`;
    }
    if (!this._devices.length) {
      return html`
        <div class="um-empty" style="padding:2rem 1rem">
          <i class="bi bi-phone"></i>
          <p>${t(`${P}.devices.empty`)}</p>
          ${this._status?.connected ? html`<p style="font-size:.8rem;opacity:.7">${t(`${P}.devices.empty_hint`)}</p>` : nothing}
        </div>`;
    }
    return html`<div class="d-flex flex-column gap-2">${this._devices.map(d => this._renderDevice(d))}</div>`;
  }

  _renderDevice(d) {
    const authorized = d.state === 'authorized';
    return html`
      <div class="connector-card" style="cursor:default">
        <div class="d-flex align-items-center gap-3" style="flex-wrap:wrap">
          <div class="connector-card-icon connector-card-icon--empty" style="width:40px;height:40px;flex:none">
            <i class="bi bi-phone"></i>
          </div>
          <div style="min-width:0;flex:1">
            <div class="d-flex align-items-center gap-2" style="flex-wrap:wrap">
              <span style="font-weight:600">${deviceLabel(d)}</span>
              <span class="badge ${authorized ? 'text-bg-success' : 'text-bg-secondary'}" style="font-size:.68rem">
                ${t(`${P}.devices.state_${d.state}`)}
              </span>
              ${this._isAdmin && d.bound_user ? html`
                <span class="badge text-bg-light" style="font-size:.68rem">
                  <i class="bi bi-person me-1"></i>${this._userName(d.bound_user)}
                </span>` : nothing}
            </div>
            <div class="text-body-secondary" style="font-size:.72rem">
              <span style="font-family:var(--font-mono,monospace)">${d.pubkey.slice(0, 16)}…</span>
              · ${t(`${P}.devices.col_last_seen`)}: ${ago(d.last_seen)}
            </div>
          </div>
          <div class="d-inline-flex gap-1 align-items-center">
            ${this._isAdmin ? html`
              <select class="form-select form-select-sm" style="width:auto"
                      .value=${this._pick[d.pubkey] || d.bound_user || ''}
                      @change=${(e) => { this._pick = { ...this._pick, [d.pubkey]: e.target.value }; }}>
                <option value="">${t(`${P}.devices.assign_to`)}</option>
                ${this._users.map(u => html`<option value=${u.id}>${u.display_name || u.username}</option>`)}
              </select>
              <button class="btn btn-sm btn-primary"
                      ?disabled=${!this._pick[d.pubkey] || this._pick[d.pubkey] === d.bound_user}
                      @click=${() => this._bind(d.pubkey)}>${t(`${P}.devices.bind`)}</button>` : nothing}
            <button class="btn btn-sm btn-outline-danger" title=${t(`${P}.devices.revoke`)} @click=${() => this._revoke(d.pubkey)}>
              <i class="bi bi-trash"></i>
            </button>
          </div>
        </div>
      </div>`;
  }

  _renderPairDialog() {
    const p = this._pair;
    if (!p) return nothing;
    const expired = p.session && p.remain <= 0;
    return html`
      <div class="um-modal-overlay" @click=${(e) => { if (e.target.classList.contains('um-modal-overlay')) this._closePairing(); }}>
        <div class="um-modal" style="max-width:420px">
          <div class="um-modal-header">
            <i class="bi bi-qr-code-scan"></i>
            <span>${t(`${P}.pair.title`)}</span>
            <button class="um-btn-icon ms-auto" @click=${() => this._closePairing()}><i class="bi bi-x-lg"></i></button>
          </div>
          <div class="um-modal-body">
            ${p.error ? html`
              <div class="alert alert-danger py-2" style="font-size:.85rem">${p.error}</div>
              ${!p.session && !p.busy ? html`
                <button class="btn btn-sm btn-primary" @click=${() => this._openPairing()}>
                  <i class="bi bi-arrow-repeat me-1"></i>${t(`${P}.pair.retry`)}</button>` : nothing}` : nothing}
            ${p.busy ? html`
              <div class="um-empty" style="padding:2rem"><i class="bi bi-hourglass-split"></i> ${t(`${P}.pair.opening`)}</div>` : nothing}
            ${p.paired ? html`
              <div class="d-flex flex-column align-items-center gap-2 py-3">
                <i class="bi bi-check-circle" style="font-size:2.5rem;color:var(--bs-success,#198754)"></i>
                <div style="font-weight:600">${t(`${P}.pair.done`)}</div>
                <div class="text-body-secondary" style="font-size:.85rem;text-align:center">${t(`${P}.pair.done_hint`)}</div>
              </div>` : nothing}
            ${p.session && !p.paired ? html`
              <div class="d-flex flex-column align-items-center gap-3">
                <img src=${p.session.url} alt=${t(`${P}.pair.qr_alt`)} width="256" height="256"
                     style="image-rendering:pixelated;border-radius:var(--radius-md,12px);${expired ? 'opacity:.25' : ''}" />
                ${expired
                  ? html`<div class="text-danger" style="font-size:.9rem"><i class="bi bi-clock-history me-1"></i>${t(`${P}.pair.expired`)}</div>`
                  : html`<div class="text-body-secondary" style="font-size:.9rem">${t(`${P}.pair.scan_within`, { n: p.remain })}</div>`}
                <div class="text-body-secondary" style="font-size:.8rem;text-align:center">${t(`${P}.pair.intro`)}</div>
              </div>` : nothing}
          </div>
          ${p.paired || p.session ? html`
          <div class="um-modal-footer">
            ${p.paired ? html`
              <button class="btn btn-sm btn-primary" @click=${() => this._closePairing()}>
                <i class="bi bi-check-lg me-1"></i>${t(`${P}.pair.close`)}</button>` : nothing}
            ${expired ? html`
              <button class="btn btn-sm btn-primary" @click=${() => this._openPairing()}>
                <i class="bi bi-arrow-repeat me-1"></i>${t(`${P}.pair.new_code`)}</button>` : nothing}
            ${p.session && !p.paired && !expired ? html`
              <button class="btn btn-sm btn-outline-secondary" @click=${() => this._closePairing()}>
                ${t(`${P}.pair.cancel`)}</button>` : nothing}
          </div>` : nothing}
        </div>
      </div>`;
  }

  _renderConfigDialog() {
    const c = this._cfg;
    if (!c) return nothing;
    const d = c.draft || {};
    return html`
      <div class="um-modal-overlay" @click=${(e) => { if (e.target.classList.contains('um-modal-overlay')) this._cfg = null; }}>
        <div class="um-modal" style="max-width:520px">
          <div class="um-modal-header">
            <i class="bi bi-gear"></i>
            <span>${t(`${P}.cfg.title`)}</span>
            <button class="um-btn-icon ms-auto" @click=${() => this._cfg = null}><i class="bi bi-x-lg"></i></button>
          </div>
          <div class="um-modal-body">
            ${c.loading ? html`<div class="um-empty" style="padding:2rem"><i class="bi bi-hourglass-split"></i></div>` : html`
              ${c.error ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:.85rem">${c.error}</div>` : nothing}
              ${c.ok ? html`<div class="alert alert-success py-2 mb-3" style="font-size:.85rem">${t(`${P}.cfg.saved`)}</div>` : nothing}

              <div class="mb-3">
                <label class="form-label">${t(`${P}.cfg.relay`)}</label>
                <select class="form-select" .value=${c.relayChoice}
                        @change=${(e) => this._cfg = { ...this._cfg, relayChoice: e.target.value, ok: false }}>
                  <option value="official" disabled>
                    ${t(`${P}.cfg.relay_official`)} — ${RELAY_OFFICIAL} (${t(`${P}.cfg.coming_soon`)})
                  </option>
                  <option value="test">${t(`${P}.cfg.relay_test`)} — ${RELAY_TEST}</option>
                  <option value="custom">${t(`${P}.cfg.relay_custom`)}</option>
                </select>
                ${c.relayChoice === 'custom' ? html`
                  <input class="form-control mt-2" style="font-family:var(--font-mono,monospace);font-size:.8rem"
                         placeholder="wss://relay.example.com/v1/ws" .value=${c.customUrl}
                         @input=${(e) => this._cfg = { ...this._cfg, customUrl: e.target.value, ok: false }} />` : nothing}
              </div>

              <div class="mb-3">
                <label class="form-label">${t(`${P}.cfg.pairing_ttl`)}</label>
                <input class="form-control" type="number" min="30" max="600" .value=${String(d.pairing_ttl ?? 300)}
                       @input=${(e) => this._patchCfg('pairing_ttl', Number(e.target.value))} />
                <div class="form-text" style="font-size:.72rem">${t(`${P}.cfg.pairing_ttl_desc`)}</div>
              </div>

              <div class="mb-3">
                <div class="form-check">
                  <input class="form-check-input" type="checkbox" id="mc-cfg-confirm"
                         .checked=${!!d.require_device_confirmation}
                         @change=${(e) => this._patchCfg('require_device_confirmation', e.target.checked)} />
                  <label class="form-check-label" for="mc-cfg-confirm">${t(`${P}.cfg.require_confirmation`)}</label>
                </div>
                <div class="form-text" style="font-size:.72rem">${t(`${P}.cfg.require_confirmation_desc`)}</div>
              </div>

              <div class="mb-1">
                <label class="form-label">${t(`${P}.cfg.notify_delay`)}</label>
                <input class="form-control" type="number" min="0" .value=${String(d.notify_delay_secs ?? 20)}
                       @input=${(e) => this._patchCfg('notify_delay_secs', Number(e.target.value))} />
                <div class="form-text" style="font-size:.72rem">${t(`${P}.cfg.notify_delay_desc`)}</div>
              </div>`}
          </div>
          <div class="um-modal-footer">
            <button class="btn btn-sm btn-outline-secondary" @click=${() => this._cfg = null}>${t(`${P}.cfg.cancel`)}</button>
            <button class="btn btn-sm btn-primary" ?disabled=${c.loading || c.busy} @click=${() => this._saveConfig()}>
              <i class="bi bi-check-lg me-1"></i>${c.busy ? t(`${P}.cfg.saving`) : t(`${P}.cfg.save`)}
            </button>
          </div>
        </div>
      </div>`;
  }
}
