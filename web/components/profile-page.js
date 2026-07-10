import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';

export class ProfilePage extends LightElement {

  static get properties() {
    return {
      _open:    { state: true },
      _me:      { state: true },
      _displayName: { state: true },
      _savingName:  { state: true },
      _nameMsg:     { state: true },
      _pwCurrent:   { state: true },
      _pwNew:       { state: true },
      _pwConfirm:   { state: true },
      _savingPw:    { state: true },
      _pwMsg:       { state: true },
    };
  }

  constructor() {
    super();
    this._open = false;
    this._me = null;
    this._displayName = '';
    this._savingName = false;
    this._nameMsg = null;
    this._pwCurrent = '';
    this._pwNew = '';
    this._pwConfirm = '';
    this._savingPw = false;
    this._pwMsg = null;
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'profile';
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) this._load();
    });
  }

  async _load() {
    try {
      const res = await fetch('/api/auth/me');
      if (res.ok) {
        this._me = await res.json();
        this._displayName = this._me.display_name ?? '';
      }
    } catch { /* ignore */ }
  }

  async _saveName() {
    if (this._savingName) return;
    this._savingName = true;
    this._nameMsg = null;
    try {
      const res = await fetch('/api/auth/profile', {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ display_name: this._displayName.trim() || null }),
      });
      if (!res.ok) throw new Error(await res.text());
      this._nameMsg = { type: 'ok', text: 'Saved.' };
      await this._load();
    } catch (e) {
      this._nameMsg = { type: 'err', text: e.message };
    } finally {
      this._savingName = false;
    }
  }

  async _savePassword() {
    if (this._savingPw) return;
    this._pwMsg = null;
    if (this._pwNew.length < 4) {
      this._pwMsg = { type: 'err', text: 'Password must be at least 4 characters.' };
      return;
    }
    if (this._pwNew !== this._pwConfirm) {
      this._pwMsg = { type: 'err', text: 'Passwords do not match.' };
      return;
    }
    this._savingPw = true;
    try {
      const res = await fetch('/api/auth/change-password', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          current_password: this._pwCurrent || null,
          new_password: this._pwNew,
        }),
      });
      if (!res.ok) throw new Error(await res.text());
      this._pwMsg = { type: 'ok', text: 'Password changed.' };
      this._pwCurrent = '';
      this._pwNew = '';
      this._pwConfirm = '';
    } catch (e) {
      this._pwMsg = { type: 'err', text: e.message };
    } finally {
      this._savingPw = false;
    }
  }

  render() {
    if (!this._open) return nothing;
    const me = this._me;

    return html`
      <div class="um-page" style="display:flex">
        <div class="um-header">
          <h2 class="um-title"><i class="bi bi-person-circle me-2"></i>Profile</h2>
        </div>
        <div style="padding:0 24px 48px;max-width:480px">

          ${me ? html`
            <div class="card mb-4" style="background:var(--card-bg);border-color:var(--card-border);border-radius:8px">
              <div class="card-body">
                <h6 class="card-title mb-3" style="font-size:.8rem;text-transform:uppercase;letter-spacing:.03em;color:var(--placeholder-color)">Account</h6>
                <div class="mb-2">
                  <label class="form-label" style="font-size:.82rem;font-weight:600">Username</label>
                  <input class="form-control" .value=${me.username} disabled />
                </div>
                <div class="mb-2">
                  <label class="form-label" style="font-size:.82rem;font-weight:600">Role</label>
                  <input class="form-control" .value=${me.role_id} disabled />
                </div>
              </div>
            </div>
          ` : nothing}

          <div class="card mb-4" style="background:var(--card-bg);border-color:var(--card-border);border-radius:8px">
            <div class="card-body">
              <h6 class="card-title mb-3" style="font-size:.8rem;text-transform:uppercase;letter-spacing:.03em;color:var(--placeholder-color)">Display name</h6>
              <div class="mb-3">
                <input class="form-control" placeholder="Your name"
                  .value=${this._displayName}
                  @input=${e => this._displayName = e.target.value} />
              </div>
              ${this._nameMsg ? html`<div class="alert alert-${this._nameMsg.type === 'ok' ? 'success' : 'danger'} py-2" style="font-size:.82rem">${this._nameMsg.text}</div>` : nothing}
              <button class="btn btn-sm btn-primary" @click=${() => this._saveName()} ?disabled=${this._savingName}>
                ${this._savingName ? 'Saving…' : 'Save'}
              </button>
            </div>
          </div>

          <div class="card mb-4" style="background:var(--card-bg);border-color:var(--card-border);border-radius:8px">
            <div class="card-body">
              <h6 class="card-title mb-3" style="font-size:.8rem;text-transform:uppercase;letter-spacing:.03em;color:var(--placeholder-color)">Change password</h6>
              ${me?.role_id === 'admin' || me?.encrypted ? html`
                <div class="mb-3">
                  <label class="form-label" style="font-size:.82rem;font-weight:600">Current password</label>
                  <input type="password" class="form-control" autocomplete="current-password"
                    .value=${this._pwCurrent}
                    @input=${e => this._pwCurrent = e.target.value} />
                </div>
              ` : nothing}
              <div class="mb-3">
                <label class="form-label" style="font-size:.82rem;font-weight:600">New password</label>
                <input type="password" class="form-control" autocomplete="new-password"
                  .value=${this._pwNew}
                  @input=${e => this._pwNew = e.target.value} />
              </div>
              <div class="mb-3">
                <label class="form-label" style="font-size:.82rem;font-weight:600">Confirm new password</label>
                <input type="password" class="form-control" autocomplete="new-password"
                  .value=${this._pwConfirm}
                  @input=${e => this._pwConfirm = e.target.value} />
              </div>
              ${this._pwMsg ? html`<div class="alert alert-${this._pwMsg.type === 'ok' ? 'success' : 'danger'} py-2" style="font-size:.82rem">${this._pwMsg.text}</div>` : nothing}
              <button class="btn btn-sm btn-primary" @click=${() => this._savePassword()} ?disabled=${this._savingPw}>
                ${this._savingPw ? 'Changing…' : 'Change password'}
              </button>
            </div>
          </div>

        </div>
      </div>
    `;
  }
}
