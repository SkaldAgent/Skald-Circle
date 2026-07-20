import { html, nothing } from 'lit';
import { LightElement }  from '../../lib/base.js';
import { t, LOCALES, getLocale, setLocale, I18nMixin } from '../../lib/i18n.js';

// Stable per-user avatar color: same user, same hue, everywhere (topbar twin).
function avatarColor(name) {
  let h = 0;
  for (let i = 0; i < name.length; i++) h = (h * 31 + name.charCodeAt(i)) >>> 0;
  return `hsl(${h % 360}, 55%, 52%)`;
}

/**
 * Mobile settings page: account card, theme + language preferences and logout.
 * The theme chain mirrors the desktop topbar (localStorage `theme` wins over
 * the OS preference, applied on `data-bs-theme`); the language goes through
 * the shared `setLocale(..., { persist: true })`, which also saves it to the
 * user profile server-side.
 */
export class SettingsPage extends I18nMixin(LightElement) {
  static properties = {
    visible: { type: Boolean },
    _me:     { state: true },
    _theme:  { state: true },
  };

  constructor() {
    super();
    this.visible = false;
    this._me     = null;
    this._theme  = document.documentElement.getAttribute('data-bs-theme') ?? 'light';
  }

  updated(changed) {
    if (changed.has('visible') && this.visible) this._loadMe();
  }

  async _loadMe() {
    try {
      const res = await fetch('/api/auth/me');
      if (res.ok) this._me = await res.json();
    } catch { /* keep whatever we have */ }
  }

  _setTheme(theme) {
    this._theme = theme;
    document.documentElement.setAttribute('data-bs-theme', theme);
    localStorage.setItem('theme', theme);
  }

  _setLocale(e) {
    setLocale(e.target.value, { persist: true });
  }

  _logout() {
    fetch('/api/auth/logout', { method: 'POST' }).then(() => window.location.reload());
  }

  render() {
    if (!this.visible) return nothing;

    const name    = this._me?.display_name || this._me?.username || '';
    const initial = name ? name.charAt(0).toUpperCase() : '?';
    const color   = this._me?.username ? avatarColor(this._me.username) : 'var(--accent)';
    const current = getLocale();

    return html`
      <div class="mobile-settings">
        <div class="mobile-section-header">
          <span class="mobile-section-title">
            <i class="bi bi-sliders"></i> ${t('mobile.nav.settings')}
          </span>
        </div>

        <div class="mobile-settings-scroll">
          <div class="settings-card">
            <div class="settings-profile">
              <div class="settings-avatar" style="background:${color}">${initial}</div>
              <div>
                <div class="settings-profile-name">${name}</div>
                <div class="settings-profile-sub">@${this._me?.username ?? ''}</div>
              </div>
            </div>
          </div>

          <div class="settings-card">
            <div class="settings-row">
              <span class="settings-row-label">
                <i class="bi ${this._theme === 'dark' ? 'bi-moon-stars' : 'bi-sun'}"></i>
                ${t('mobile.settings.theme')}
              </span>
              <div class="settings-segment">
                <button class="${this._theme === 'light' ? 'active' : ''}"
                        title=${t('mobile.settings.light')}
                        @click=${() => this._setTheme('light')}>
                  <i class="bi bi-sun"></i>
                </button>
                <button class="${this._theme === 'dark' ? 'active' : ''}"
                        title=${t('mobile.settings.dark')}
                        @click=${() => this._setTheme('dark')}>
                  <i class="bi bi-moon-stars"></i>
                </button>
              </div>
            </div>
            <div class="settings-row">
              <span class="settings-row-label">
                <i class="bi bi-translate"></i>
                ${t('mobile.settings.language')}
              </span>
              <select class="settings-lang-select" @change=${(e) => this._setLocale(e)}>
                ${LOCALES.map(l => html`
                  <option value=${l.id} ?selected=${l.id === current}>${l.label}</option>
                `)}
              </select>
            </div>
          </div>

          <button class="settings-logout" @click=${() => this._logout()}>
            <i class="bi bi-box-arrow-right"></i> ${t('topbar.logout')}
          </button>
        </div>
      </div>
    `;
  }
}

customElements.define('settings-page', SettingsPage);
