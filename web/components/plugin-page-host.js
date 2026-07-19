import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';
import { t } from '../lib/i18n.js';

// Host for plugin-contributed pages (`#plugin/<plugin_id>/<page_id>`).
//
// The frontend knows nothing about what a plugin page does: on navigation it
// dynamic-imports the fragment ES module the plugin serves from its own router
// (`/api/plugin/<id>/<entry>`), registers its default-exported HTMLElement
// class as a custom element, and mounts it with the `plugin-id` attribute set.
// The fragment talks to its backend only through `/api/plugin/<id>/…` and runs
// with the full session privileges — plugins are trusted (they ship in the
// binary). See `Plugin::web_pages` in core-api for the fragment contract.
export class PluginPageHost extends LightElement {

  static get properties() {
    return {
      _open:    { state: true },
      _route:   { state: true },   // "plugin/<plugin_id>/<page_id>" while open
      _error:   { state: true },
      _loading: { state: true },
    };
  }

  constructor() {
    super();
    this._open = false;
    this._route = null;
    this._error = null;
    this._loading = false;
    this._mounted = null;   // currently mounted fragment element
  }

  connectedCallback() {
    super.connectedCallback();
    this.style.display = 'none';
    window.addEventListener('llm-page-change', (e) => {
      const page = e.detail.page || '';
      if (page.startsWith('plugin/')) {
        this._openPage(page);
      } else {
        this._open = false;
        this._route = null;
        this.style.display = 'none';
      }
    });
  }

  async _openPage(route) {
    this._open = true;
    this.style.display = 'flex';
    if (route === this._route) return;
    this._route = route;
    this._error = null;
    this._loading = true;

    const [, pluginId, pageId] = route.split('/');
    const tag = `skald-plugin-${pluginId}-${pageId}`;
    try {
      if (!customElements.get(tag)) {
        const entry_url = await this._resolveEntry(pluginId, pageId);
        const mod = await import(/* @vite-ignore */ entry_url);
        const cls = mod.default;
        if (!cls || !(cls.prototype instanceof HTMLElement)) {
          throw new Error('fragment must default-export an HTMLElement class');
        }
        customElements.define(tag, cls);
      }
      const el = document.createElement(tag);
      el.setAttribute('plugin-id', pluginId);
      if (this._mounted) this._mounted.remove();
      this._mounted = el;
    } catch (e) {
      this._error = e.message || String(e);
      if (this._mounted) { this._mounted.remove(); this._mounted = null; }
    } finally {
      this._loading = false;
    }
  }

  async _resolveEntry(pluginId, pageId) {
    const res = await fetch('/api/plugins/pages');
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    const pages = await res.json();
    const page = pages.find(p => p.plugin_id === pluginId && p.page_id === pageId);
    if (!page) throw new Error(t('plugin_page.unavailable'));
    return page.entry_url;
  }

  render() {
    if (!this._open) return nothing;
    return html`
      ${this._loading ? html`<div class="p-4 text-body-secondary">${t('plugin_page.loading')}</div>` : nothing}
      ${this._error ? html`<div class="p-4 text-danger">${this._error}</div>` : nothing}
      ${this._mounted && !this._error ? this._mounted : nothing}
    `;
  }
}
