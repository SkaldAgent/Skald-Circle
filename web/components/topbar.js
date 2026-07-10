import { html }         from 'lit';
import { LightElement } from '../lib/base.js';

export class AppTopbar extends LightElement {
  static properties = {
    _theme:           { state: true },
    _copilotCollapsed: { state: true },
  };

  constructor() {
    super();
    this._theme            = document.documentElement.getAttribute('data-bs-theme') ?? 'light';
    this._copilotCollapsed = false;
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('copilot-collapsed', (e) => {
      this._copilotCollapsed = e.detail.collapsed;
    });
  }

  _toggleTheme() {
    const next = this._theme === 'dark' ? 'light' : 'dark';
    this._theme = next;
    document.documentElement.setAttribute('data-bs-theme', next);
    localStorage.setItem('theme', next);
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
    `;
  }
}
