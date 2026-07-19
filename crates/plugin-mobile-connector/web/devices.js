// Mobile-connector "Mobile devices" console (page_id `devices`).
//
// Lists every paired device with its state and bound user, and lets an admin
// reassign a device to another user (`POST /devices/bind`) or revoke it
// (`POST /devices/revoke`). The user directory for the reassign dropdown comes
// from the host `/api/users` (the fragment runs with the admin's session).
// Default-exports the element class; the host registers it.
import { html, nothing } from 'lit';
import { MobileBase, jf, ago, deviceLabel, t } from './common.js';

const P = 'plugin.mobile-connector';

export default class MobileDevicesPage extends MobileBase {
  static get properties() {
    return {
      _devices: { state: true },   // [] | null (loading)
      _users:   { state: true },   // [{id, username, display_name}]
      _error:   { state: true },
      _pick:    { state: true },   // { [pubkey]: user_id } reassign selections
    };
  }

  constructor() {
    super();
    this._devices = null;
    this._users = [];
    this._error = null;
    this._pick = {};
    this._poll = null;
  }

  connectedCallback() {
    super.connectedCallback();
    this._load();
    this._poll = setInterval(() => this._load(true), 5000);
  }

  disconnectedCallback() {
    super.disconnectedCallback();
    if (this._poll) { clearInterval(this._poll); this._poll = null; }
  }

  async _load(quiet = false) {
    if (!quiet) this._error = null;
    try {
      const [d, u] = await Promise.all([
        jf(`${this.api}/devices`),
        this._users.length ? Promise.resolve({ list: this._users }) : jf('/api/users').then(list => ({ list })),
      ]);
      this._devices = d.devices || [];
      if (u.list) this._users = u.list;
    } catch (e) {
      if (!quiet) this._error = e.message;
    }
  }

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

  render() {
    const loading = this._devices === null && !this._error;
    return html`
      <div class="um-page">
        <div class="um-header d-flex justify-content-between align-items-center">
          <h2 class="um-title"><i class="bi bi-phone me-2"></i>${t(`${P}.devices.title`)}</h2>
          <button class="btn btn-sm btn-outline-secondary" @click=${() => this._load()}>
            <i class="bi bi-arrow-repeat me-1"></i>${t(`${P}.devices.refresh`)}</button>
        </div>
        <div style="padding:0 1.25rem 1.5rem">
          ${this._error ? html`<div class="alert alert-danger py-2" style="font-size:.85rem">${this._error}</div>` : nothing}
          ${loading ? html`<div class="um-empty"><i class="bi bi-hourglass-split"></i> ${t(`${P}.devices.loading`)}</div>` : this._renderList()}
        </div>
      </div>`;
  }

  _renderList() {
    const rows = this._devices || [];
    if (!rows.length) {
      return html`<div class="um-empty" style="padding:1rem">
        <i class="bi bi-phone"></i><p>${t(`${P}.devices.empty`)}</p>
        <p style="font-size:.8rem;opacity:.7">${t(`${P}.devices.empty_hint`)}</p>
      </div>`;
    }
    return html`
      <div class="table-responsive">
        <table class="table align-middle" style="font-size:.88rem">
          <thead><tr>
            <th>${t(`${P}.devices.col_device`)}</th><th>${t(`${P}.devices.col_state`)}</th><th>${t(`${P}.devices.col_bound`)}</th><th>${t(`${P}.devices.col_last_seen`)}</th><th class="text-end">${t(`${P}.devices.col_actions`)}</th>
          </tr></thead>
          <tbody>${rows.map(d => this._renderRow(d))}</tbody>
        </table>
      </div>`;
  }

  _renderRow(d) {
    const authorized = d.state === 'authorized';
    return html`
      <tr>
        <td>
          <div>${deviceLabel(d)}</div>
          <div class="text-body-secondary" style="font-size:.72rem; font-family:var(--font-mono,monospace)">
            ${d.pubkey.slice(0, 16)}…</div>
        </td>
        <td>
          <span class="badge ${authorized ? 'text-bg-success' : 'text-bg-secondary'}">${t(`${P}.devices.state_${d.state}`)}</span>
        </td>
        <td>${d.bound_user ? this._userName(d.bound_user) : html`<span class="text-body-secondary">—</span>`}</td>
        <td class="text-body-secondary">${ago(d.last_seen)}</td>
        <td class="text-end">
          <div class="d-inline-flex gap-1 align-items-center">
            <select class="form-select form-select-sm" style="width:auto"
                    .value=${this._pick[d.pubkey] || d.bound_user || ''}
                    @change=${(e) => { this._pick = { ...this._pick, [d.pubkey]: e.target.value }; }}>
              <option value="">${t(`${P}.devices.assign_to`)}</option>
              ${this._users.map(u => html`<option value=${u.id}>${u.display_name || u.username}</option>`)}
            </select>
            <button class="btn btn-sm btn-primary"
                    ?disabled=${!this._pick[d.pubkey] || this._pick[d.pubkey] === d.bound_user}
                    @click=${() => this._bind(d.pubkey)}>${t(`${P}.devices.bind`)}</button>
            <button class="btn btn-sm btn-outline-danger" @click=${() => this._revoke(d.pubkey)}>
              <i class="bi bi-trash"></i></button>
          </div>
        </td>
      </tr>`;
  }
}
