import { html } from 'lit';
import { LightElement } from '../lib/base.js';
import { t, I18nMixin, LOCALES, getLocale, setLocale } from '../lib/i18n.js';

export class SetupPage extends I18nMixin(LightElement) {

  static get properties() {
    return {
      _username:  { state: true },
      _password:  { state: true },
      _confirm:   { state: true },
      _encrypted: { state: true },
      _locale:    { state: true },
      _profiles:  { state: true },
      _profile:   { state: true },
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
    this._locale    = getLocale();
    this._profiles  = [];
    this._profile   = 'family';
    this._error     = null;
    this._busy      = false;
  }

  connectedCallback() {
    super.connectedCallback();
    this._loadProfiles();
  }

  async _loadProfiles() {
    try {
      const res = await fetch('/api/setup/profiles');
      if (!res.ok) return;
      const list = await res.json();
      if (Array.isArray(list) && list.length) {
        this._profiles = list;
        this._profile  = list[0].id;
      }
    } catch { /* one preset ships; a failed fetch just keeps the default */ }
  }

  _submit(e) {
    e.preventDefault();
    if (this._busy) return;

    this._error = null;

    if (!this._username.trim()) {
      this._error = t('setup.username');
      return;
    }
    if (this._password.length < 4) {
      this._error = t('setup.pw.short');
      return;
    }
    if (this._password !== this._confirm) {
      this._error = t('setup.pw.mismatch');
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
          locale:    this._locale,
          profile:   this._profile,
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
      this._error = t('setup.network');
    } finally {
      this._busy = false;
    }
  }

  render() {
    const btnLabel = this._busy
      ? html`<span class="setup-spinner"></span>${t('setup.creating')}`
      : t('setup.submit');

    return html`
      <div class="setup-page">
        <form class="setup-card" @submit=${this._submit} autocomplete="off">
          <div class="setup-logo">
            <img src="/assets/icons/icon-192.png" alt="Skald" />
          </div>
          <h1 class="setup-title">${t('setup.title')}</h1>
          <p class="setup-subtitle">${t('setup.subtitle')}</p>

          ${this._error ? html`<div class="setup-error">${this._error}</div>` : null}

          ${this._profiles.length > 1 ? html`
            <div class="mb-3">
              <label class="form-label">${t('setup.profile')}</label>
              <select
                class="form-select"
                .value=${this._profile}
                @change=${e => this._profile = e.target.value}
                ?disabled=${this._busy}>
                ${this._profiles.map(p => html`
                  <option value=${p.id} ?selected=${this._profile === p.id}>${p.label}</option>
                `)}
              </select>
            </div>
          ` : null}

          <div class="mb-3">
            <label class="form-label">${t('login.username')}</label>
            <input
              type="text"
              class="form-control"
              .value=${this._username}
              @input=${e => this._username = e.target.value}
              ?disabled=${this._busy}
              required />
          </div>

          <div class="mb-3">
            <label class="form-label">${t('login.password')}</label>
            <input
              type="password"
              class="form-control"
              .value=${this._password}
              @input=${e => this._password = e.target.value}
              ?disabled=${this._busy}
              required />
          </div>

          <div class="mb-3">
            <label class="form-label">${t('setup.confirm')}</label>
            <input
              type="password"
              class="form-control"
              .value=${this._confirm}
              @input=${e => this._confirm = e.target.value}
              ?disabled=${this._busy}
              required />
          </div>

          <div class="mb-3">
            <label class="form-label">${t('setup.language')}</label>
            <select
              class="form-select"
              .value=${this._locale}
              @change=${e => { this._locale = e.target.value; setLocale(this._locale); }}
              ?disabled=${this._busy}>
              ${LOCALES.map(l => html`
                <option value=${l.id} ?selected=${this._locale === l.id}>${l.label}</option>
              `)}
            </select>
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
              ${t('setup.encrypt')}
            </label>
          </div>

          ${this._encrypted ? html`
            <div class="setup-warn">
              <strong>${t('setup.warn.strong')}</strong> ${t('setup.warn')}
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
