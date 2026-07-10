import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';

export class AppTopbar extends LightElement {
  static properties = {
    _theme:            { state: true },
    _copilotCollapsed: { state: true },
    _menuOpen:         { state: true },
    _me:               { state: true },
  };

  constructor() {
    super();
    this._theme            = document.documentElement.getAttribute('data-bs-theme') ?? 'light';
    this._copilotCollapsed = false;
    this._menuOpen         = false;
    this._me               = null;
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('copilot-collapsed', (e) => {
      this._copilotCollapsed = e.detail.collapsed;
    });
    this._loadMe();
    document.addEventListener('click', (e) => {
      if (this._menuOpen && !e.composedPath().includes(this)) {
        this._menuOpen = false;
      }
    });
  }

  async _loadMe() {
    try {
      const res = await fetch('/api/auth/me');
      if (res.ok) this._me = await res.json();
    } catch { /* ignore */ }
  }

  _toggleTheme() {
    const next = this._theme === 'dark' ? 'light' : 'dark';
    this._theme = next;
    document.documentElement.setAttribute('data-bs-theme', next);
    localStorage.setItem('theme', next);
  }

  _toggleMenu(e) {
    e.stopPropagation();
    this._menuOpen = !this._menuOpen;
  }

  _goProfile() {
    this._menuOpen = false;
    history.pushState({ page: 'profile' }, '', '#profile');
    window.dispatchEvent(new CustomEvent('llm-page-change', { detail: { page: 'profile' } }));
  }

  _logout() {
    this._menuOpen = false;
    fetch('/api/auth/logout', { method: 'POST' }).then(() => window.location.reload());
  }

  get _initial() {
    const name = this._me?.display_name || this._me?.username || '?';
    return name.charAt(0).toUpperCase();
  }

  render() {
    const isDark = this._theme === 'dark';
    return html`
      <span class="topbar-title">Skald</span>
      <span class="topbar-spacer"></span>
      ${this._copilotCollapsed ? html`
        <button class="topbar-copilot-btn" title="Open copilot"
                @click=${() => window.dispatchEvent(new CustomEvent('copilot-open'))}>
          <i class="bi bi-stars"></i>
        </button>
      ` : ''}
      <button class="topbar-theme-btn" title="${isDark ? 'Switch to light mode' : 'Switch to dark mode'}"
              @click=${() => this._toggleTheme()}>
        <i class="bi ${isDark ? 'bi-sun' : 'bi-moon-stars'}"></i>
      </button>
      <div class="topbar-profile-wrapper">
        <button class="topbar-avatar" title="Account" @click=${(e) => this._toggleMenu(e)}>
          ${this._initial}
        </button>
        ${this._menuOpen ? html`
          <div class="topbar-dropdown">
            <div class="topbar-dropdown-header">
              <div class="topbar-dropdown-name">${this._me?.display_name || this._me?.username || ''}</div>
              <div class="topbar-dropdown-sub">@${this._me?.username || ''}</div>
            </div>
            <button class="topbar-dropdown-item" @click=${() => this._goProfile()}>
              <i class="bi bi-person"></i> Profile
            </button>
            <button class="topbar-dropdown-item topbar-dropdown-logout" @click=${() => this._logout()}>
              <i class="bi bi-box-arrow-right"></i> Logout
            </button>
          </div>
        ` : nothing}
      </div>
    `;
  }
}
