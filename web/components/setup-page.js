import { html } from 'lit';
import { LightElement } from '../lib/base.js';

export class SetupPage extends LightElement {

  static get properties() {
    return {
      _username:  { state: true },
      _password:  { state: true },
      _confirm:   { state: true },
      _encrypted: { state: true },
      _error:     { state: true },
      _busy:      { state: true },
    };
  }

  constructor() {
    super();
    this._username  = '';
    this._password  = '';
    this._confirm   = '';
    this._encrypted = true;
    this._error     = null;
    this._busy      = false;
  }

  _submit(e) {
    e.preventDefault();
    if (this._busy) return;

    this._error = null;

    if (!this._username.trim()) {
      this._error = 'Choose a username.';
      return;
    }
    if (this._password.length < 4) {
      this._error = 'Password must be at least 4 characters.';
      return;
    }
    if (this._password !== this._confirm) {
      this._error = 'The two passwords do not match.';
      return;
    }

    this._doCreate();
  }

  async _doCreate() {
    this._busy = true;
    try {
      const res = await fetch('/api/setup/user', {
        method:  'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({
          username:  this._username.trim(),
          password:  this._password,
          encrypted: this._encrypted,
        }),
      });
      if (!res.ok) {
        const txt = await res.text();
        this._error = txt || `Request failed (${res.status}).`;
        return;
      }
      // First user created — reload into the app.
      window.location.reload();
    } catch {
      this._error = 'Network error — please try again.';
    } finally {
      this._busy = false;
    }
  }

  render() {
    const btnLabel = this._busy
      ? html`<span class="setup-spinner"></span>Creating…`
      : 'Create account';

    return html`
      <div class="setup-page">
        <form class="setup-card" @submit=${this._submit} autocomplete="off">
          <div class="setup-logo">
            <img src="/assets/icons/icon-192.png" alt="Skald" />
          </div>
          <h1 class="setup-title">Welcome to Skald</h1>
          <p class="setup-subtitle">Create the admin account to get started.</p>

          ${this._error ? html`<div class="setup-error">${this._error}</div>` : null}

          <div class="mb-3">
            <label class="form-label">Username</label>
            <input
              type="text"
              class="form-control"
              .value=${this._username}
              @input=${e => this._username = e.target.value}
              ?disabled=${this._busy}
              required />
          </div>

          <div class="mb-3">
            <label class="form-label">Password</label>
            <input
              type="password"
              class="form-control"
              .value=${this._password}
              @input=${e => this._password = e.target.value}
              ?disabled=${this._busy}
              required />
          </div>

          <div class="mb-3">
            <label class="form-label">Confirm password</label>
            <input
              type="password"
              class="form-control"
              .value=${this._confirm}
              @input=${e => this._confirm = e.target.value}
              ?disabled=${this._busy}
              required />
          </div>

          <div class="form-check">
            <input
              class="form-check-input"
              type="checkbox"
              id="encrypt-chk"
              .checked=${this._encrypted}
              @change=${e => this._encrypted = e.target.checked}
              ?disabled=${this._busy} />
            <label class="form-check-label" for="encrypt-chk">
              Encrypt my conversation history
            </label>
          </div>

          ${this._encrypted ? html`
            <div class="setup-warn">
              <strong>Warning:</strong> your password derives the encryption key.
              If you forget it, <strong>your entire conversation history will be
              permanently lost</strong> — there is no recovery.
            </div>
          ` : null}

          <button type="submit" class="btn btn-primary setup-submit" ?disabled=${this._busy}>
            ${btnLabel}
          </button>
        </form>
      </div>
    `;
  }
}
