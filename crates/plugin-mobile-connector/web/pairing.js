// Mobile-connector "Pair a device" console (page_id `pairing`).
//
// Opens a pairing window on the plugin (`POST /pairing`), shows the QR the phone
// scans, and counts down to expiry. A device that pairs in this window is
// auto-bound to the admin who opened it (server-side, on `ClientPaired`) — so it
// is usable on the phone immediately and can be reassigned later from the
// Devices page. Default-exports the element class; the host registers it.
import { html, nothing } from 'lit';
import { MobileBase, jf, t } from './common.js';

const P = 'plugin.mobile-connector';

export default class MobilePairingPage extends MobileBase {
  static get properties() {
    return {
      _session: { state: true },   // { url, code, expires_at } | null
      _remain:  { state: true },   // seconds until expiry
      _busy:    { state: true },
      _error:   { state: true },
    };
  }

  constructor() {
    super();
    this._session = null;
    this._remain = 0;
    this._busy = false;
    this._error = null;
    this._timer = null;
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    this._stopTimer();
    // Best-effort close so a forgotten window does not linger.
    if (this._session) jf(`${this.api}/pairing`, { method: 'DELETE' }).catch(() => {});
  }

  _stopTimer() { if (this._timer) { clearInterval(this._timer); this._timer = null; } }

  _startTimer() {
    this._stopTimer();
    const tick = () => {
      const remain = Math.max(0, Math.round((this._session.expires_at - Date.now()) / 1000));
      this._remain = remain;
      if (remain <= 0) { this._stopTimer(); }
    };
    tick();
    this._timer = setInterval(tick, 1000);
  }

  async _open() {
    this._busy = true;
    this._error = null;
    try {
      this._session = await jf(`${this.api}/pairing`, { method: 'POST', body: JSON.stringify({}) });
      this._startTimer();
    } catch (e) {
      this._error = e.message;
      this._session = null;
    } finally {
      this._busy = false;
    }
  }

  async _stop() {
    this._stopTimer();
    const had = this._session;
    this._session = null;
    if (had) { try { await jf(`${this.api}/pairing`, { method: 'DELETE' }); } catch { /* ignore */ } }
  }

  render() {
    const expired = this._session && this._remain <= 0;
    return html`
      <div class="um-page">
        <div class="um-header">
          <h2 class="um-title"><i class="bi bi-qr-code me-2"></i>${t(`${P}.pairing.title`)}</h2>
        </div>
        <div style="padding:0 1.25rem 1.5rem; max-width:640px">
          ${this._error ? html`<div class="alert alert-danger py-2" style="font-size:.85rem">${this._error}</div>` : nothing}

          ${!this._session ? html`
            <p class="text-body-secondary" style="font-size:.9rem">
              ${t(`${P}.pairing.intro`)}
            </p>
            <button class="btn btn-primary" ?disabled=${this._busy} @click=${() => this._open()}>
              <i class="bi bi-qr-code-scan me-1"></i>${this._busy ? t(`${P}.pairing.opening`) : t(`${P}.pairing.open`)}
            </button>
          ` : html`
            <div class="d-flex flex-column align-items-center gap-3 p-3"
                 style="border:1px solid var(--border-color,#ddd); border-radius:var(--radius-md,12px)">
              <img src=${this._session.url} alt=${t(`${P}.pairing.qr_alt`)} width="256" height="256"
                   style="image-rendering:pixelated; ${expired ? 'opacity:.25' : ''}" />
              ${expired
                ? html`<div class="text-danger" style="font-size:.9rem"><i class="bi bi-clock-history me-1"></i>${t(`${P}.pairing.expired`)}</div>`
                : html`<div class="text-body-secondary" style="font-size:.9rem">
                    ${t(`${P}.pairing.scan_within`, { n: this._remain })}
                  </div>`}
              <div class="d-flex gap-2">
                ${expired
                  ? html`<button class="btn btn-primary btn-sm" @click=${() => this._open()}>
                           <i class="bi bi-arrow-repeat me-1"></i>${t(`${P}.pairing.new_code`)}</button>`
                  : html`<button class="btn btn-outline-secondary btn-sm" @click=${() => this._stop()}>
                           <i class="bi bi-x-lg me-1"></i>${t(`${P}.pairing.close`)}</button>`}
              </div>
            </div>
          `}
        </div>
      </div>`;
  }
}
