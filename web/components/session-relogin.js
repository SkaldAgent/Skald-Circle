import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';
import { t, I18nMixin } from '../lib/i18n.js';
import { notifySessionRestored } from '../lib/session-expiry.js';

/**
 * Re-login dialog for a session that died under an open tab.
 *
 * Sessions live in the server's RAM (blueprint §9), so a restart logs everyone
 * out while their browser keeps sending a cookie nobody recognises. The obvious
 * answer — bounce to the login screen — throws away everything the page was
 * holding, and the composer's half-written message with it. So the session is
 * renewed **in place**: a modal over the page the user was already on, one
 * password field, and on success the app carries on with its state intact (the
 * chat reconnects on `auth-restored`).
 *
 * Not dismissible, deliberately: with no session nothing on the page works, and
 * a dialog you can wave away would leave a UI that silently fails every action.
 */
export class SessionRelogin extends I18nMixin(LightElement) {

  static get properties() {
    return {
      _open:     { state: true },
      _username: { state: true },
      _password: { state: true },
      _error:    { state: true },
      _busy:     { state: true },
    };
  }

  constructor() {
    super();
    this._open     = false;
    this._username = '';
    this._password = '';
    this._error    = null;
    this._busy     = false;
  }

  /** Raise the dialog, prefilling the last username known to this browser. */
  open() {
    if (this._open) return;
    let last = '';
    try { last = localStorage.getItem('skald.last_user') ?? ''; } catch { /* private mode */ }
    this._username = last;
    this._password = '';
    this._error    = null;
    this._open     = true;
    // Focus the field the user actually has to fill: the password when we
    // already know who they are, the username otherwise.
    this.updateComplete.then(() => {
      const sel = last ? 'input[type="password"]' : 'input[type="text"]';
      this.querySelector(sel)?.focus();
    });
  }

  _submit(e) {
    e.preventDefault();
    if (this._busy) return;
    this._error = null;
    if (!this._username.trim() || !this._password) {
      this._error = t('login.missing');
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
        this._error = t('login.error');
        return;
      }
      try { localStorage.setItem('skald.last_user', this._username.trim()); } catch { /* private mode */ }
      this._password = '';
      this._open     = false;
      // The new cookie is live: tell the app to pick its connections back up.
      notifySessionRestored();
    } catch {
      this._error = t('login.network');
    } finally {
      this._busy = false;
    }
  }

  render() {
    if (!this._open) return nothing;
    const btnLabel = this._busy
      ? html`<span class="login-spinner"></span>${t('login.signing')}`
      : t('login.submit');

    return html`
      <div class="relogin-backdrop">
        <form class="login-card relogin-card" @submit=${this._submit} autocomplete="on">
          <h2 class="login-title relogin-title">${t('login.expired.title')}</h2>
          <p class="login-subtitle">${t('login.expired')}</p>

          ${this._error ? html`<div class="login-error">${this._error}</div>` : null}

          <div class="mb-3">
            <label class="form-label">${t('login.username')}</label>
            <input
              type="text"
              class="form-control"
              autocomplete="username"
              .value=${this._username}
              @input=${e => this._username = e.target.value}
              ?disabled=${this._busy} />
          </div>

          <div class="mb-3">
            <label class="form-label">${t('login.password')}</label>
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

/**
 * Mount the dialog and wire it to `auth-expired`.
 *
 * The element appends itself to `<body>` rather than living in the two shells'
 * HTML: it belongs to whichever page happens to be open, and desktop and mobile
 * would otherwise each have to remember to declare it.
 */
export function installSessionRelogin() {
  if (!customElements.get('session-relogin')) {
    customElements.define('session-relogin', SessionRelogin);
  }
  let el = null;
  window.addEventListener('auth-expired', () => {
    if (!el) {
      el = document.createElement('session-relogin');
      document.body.appendChild(el);
    }
    el.open();
  });
}
