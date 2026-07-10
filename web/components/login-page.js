import { html } from 'lit';
import { LightElement } from '../lib/base.js';

export class LoginPage extends LightElement {

  static get properties() {
    return {
      _username: { state: true },
      _password: { state: true },
      _error:    { state: true },
      _busy:     { state: true },
    };
  }

  constructor() {
    super();
    this._username = '';
    this._password = '';
    this._error    = null;
    this._busy     = false;
  }

  _submit(e) {
    e.preventDefault();
    if (this._busy) return;

    this._error = null;

    if (!this._username.trim() || !this._password) {
      this._error = 'Enter your username and password.';
      return;
    }

    this._doLogin();
  }

  async _doLogin() {
    this._busy = true;
    try {
      const res = await fetch('/api/auth/login', {
        method:  'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({
          username: this._username.trim(),
          password: this._password,
        }),
      });
      if (!res.ok) {
        this._error = 'Invalid username or password.';
        return;
      }
      // Logged in — reload into the app.
      window.location.reload();
    } catch {
      this._error = 'Network error — please try again.';
    } finally {
      this._busy = false;
    }
  }

  render() {
    const btnLabel = this._busy
      ? html`<span class="login-spinner"></span>Signing in…`
      : 'Sign in';

    return html`
      <div class="login-page">
        <form class="login-card" @submit=${this._submit} autocomplete="on">
          <div class="login-logo">
            <img src="/assets/icons/icon-192.png" alt="Skald" />
          </div>
          <h1 class="login-title">Welcome back</h1>
          <p class="login-subtitle">Sign in to your account.</p>

          ${this._error ? html`<div class="login-error">${this._error}</div>` : null}

          <div class="mb-3">
            <label class="form-label">Username</label>
            <input
              type="text"
              class="form-control"
              autocomplete="username"
              .value=${this._username}
              @input=${e => this._username = e.target.value}
              ?disabled=${this._busy} />
          </div>

          <div class="mb-3">
            <label class="form-label">Password</label>
            <input
              type="password"
              class="form-control"
              autocomplete="current-password"
              .value=${this._password}
              @input=${e => this._password = e.target.value}
              ?disabled=${this._busy} />
          </div>

          <button type="submit" class="btn btn-primary login-submit" ?disabled=${this._busy}>
            ${btnLabel}
          </button>
        </form>
      </div>
    `;
  }
}
