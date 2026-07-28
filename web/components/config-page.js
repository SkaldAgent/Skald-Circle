import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';
import { t }            from '../lib/i18n.js';
import { ConfigFormController, maybeT, propKeyId } from './shared/config-form.js';

// Sets whose labels this page ships translations for. A set with no slug falls
// back to the backend's own English text, which is also what happens to a newly
// added one until it is translated.
function _configSetSlug(name) {
  const slugs = {
    'Interface':  'interface',
    'Compaction': 'compaction',
  };
  return slugs[name] ?? null;
}

export class ConfigPage extends LightElement {
  static properties = {
    _open:       { state: true },
    _properties: { state: true },
    _error:      { state: true },
    _debugMode:    { state: true },
    _debugLoading: { state: true },
  };

  constructor() {
    super();
    this._open       = false;
    this._properties = [];
    this._error      = null;
    this._debugMode    = false;
    this._debugLoading = true;
    // Values, in-flight saves and the saved-flash live in the shared controller,
    // which also owns the write path (see `shared/config-form.js`).
    this._form = new ConfigFormController(() => this.requestUpdate());
  }

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'config';
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) { this._load(); this._loadDebugMode(); }
    });
  }

  disconnectedCallback() {
    window.removeEventListener('locale-changed', this.__onLocaleChanged);
    super.disconnectedCallback();
  }

  async _loadDebugMode() {
    try {
      const res = await fetch('/api/dev/debug_mode');
      if (!res.ok) throw new Error();
      const data = await res.json();
      this._debugMode = data.enabled;
    } catch {
      // ignore, keep current value
    } finally {
      this._debugLoading = false;
    }
  }

  async _toggleDebugMode() {
    const next = !this._debugMode;
    this._debugMode = next;
    try {
      const res = await fetch('/api/dev/debug_mode', {
        method:  'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({ enabled: next }),
      });
      if (!res.ok) throw new Error();
      window.dispatchEvent(new CustomEvent('debug-mode-change', { detail: { enabled: next } }));
    } catch {
      this._debugMode = !next;
    }
  }

  async _load() {
    this._error = null;
    try {
      const res = await fetch('/api/config');
      if (!res.ok) throw new Error(await res.text());
      const data = await res.json();
      this._properties = data.sets ?? [];
      this._form.seedFromSets(this._properties);
    } catch (e) {
      this._error = e.message;
    }
  }

  _renderSet(set) {
    const slug  = _configSetSlug(set.name);
    const sName = slug ? maybeT(`config.set.${slug}.name`, set.name) : set.name;
    const sDesc = slug ? maybeT(`config.set.${slug}.desc`, set.description) : set.description;
    return html`
      <div class="config-set">
        <div class="config-set-header">
          <div class="config-set-name">${sName}</div>
          <div class="config-set-desc">${sDesc}</div>
        </div>
        ${this._form.renderRows(set.properties, p => {
          const pk = propKeyId(p.key);
          return {
            name:        maybeT(`config.prop.${pk}.name`, p.name),
            description: maybeT(`config.prop.${pk}.desc`, p.description),
          };
        })}
      </div>`;
  }

  render() {
    return html`
      <div class="config-page">
        <div class="config-page-header">
          <h2 class="llm-page-title">${t('config.title')}</h2>
        </div>

        ${this._error ? html`
          <div class="alert alert-danger">${this._error}</div>` : nothing}

        ${this._properties.length === 0 && !this._error ? html`
          <p class="text-muted mt-2">${t('config.loading')}</p>` : nothing}

        <div class="config-sets">
          ${this._properties.map(s => this._renderSet(s))}
        </div>

        <div class="config-set">
          <div class="config-set-header">
            <div class="config-set-name">${t('config.developer')}</div>
            <div class="config-set-desc"></div>
          </div>
          <div class="config-rows">
            <div class="config-row">
              <div class="config-row-meta">
                <div class="config-row-name">${t('config.debug')}</div>
                <div class="config-row-desc">${t('config.debug.desc')}</div>
              </div>
              <div class="config-row-control">
                <div class="form-check form-switch config-bool-switch">
                  <input class="form-check-input" type="checkbox" role="switch"
                         id="cfg-debug-mode"
                         .checked=${this._debugMode}
                         ?disabled=${this._debugLoading}
                         @change=${() => this._toggleDebugMode()} />
                  <label class="form-check-label" for="cfg-debug-mode">
                    ${this._debugMode ? t('config.enabled') : t('config.disabled')}
                  </label>
                </div>
              </div>
            </div>
          </div>
        </div>
      </div>`;
  }
}
