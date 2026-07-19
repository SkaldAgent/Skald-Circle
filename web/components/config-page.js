import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';
import { t }            from '../lib/i18n.js';

function _maybeT(key, fallback) {
  const v = t(key);
  return v !== key ? v : fallback;
}

function _configSetSlug(name) {
  const slugs = {
    'Interface': 'interface',
    'TIC Agent': 'tic_agent',
  };
  return slugs[name] ?? null;
}

function _propKeyId(propKey) {
  return propKey.replace(/\./g, '__');
}

export class ConfigPage extends LightElement {
  static properties = {
    _open:       { state: true },
    _properties: { state: true },
    _values:     { state: true },   // { [key]: string }
    _saving:     { state: true },   // Set<key>
    _saved:      { state: true },   // Set<key>  (brief flash)
    _error:      { state: true },
    _debugMode:    { state: true },
    _debugLoading: { state: true },
  };

  constructor() {
    super();
    this._open       = false;
    this._properties = [];
    this._values     = {};
    this._saving     = new Set();
    this._saved      = new Set();
    this._error      = null;
    this._debugMode    = false;
    this._debugLoading = true;
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
      const vals = {};
      for (const s of this._properties)
        for (const p of s.properties) vals[p.key] = p.value ?? '';
      this._values = vals;
    } catch (e) {
      this._error = e.message;
    }
  }

  _setValue(key, val) {
    this._values = { ...this._values, [key]: val };
  }

  async _save(prop) {
    const key   = prop.key;
    const value = this._values[key] ?? '';

    this._saving = new Set([...this._saving, key]);
    this.requestUpdate();

    try {
      const res = await fetch(`/api/config/${encodeURIComponent(key)}`, {
        method:  'PUT',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({ value }),
      });
      if (!res.ok) throw new Error(await res.text());

      this._saved = new Set([...this._saved, key]);
      setTimeout(() => {
        this._saved = new Set([...this._saved].filter(k => k !== key));
      }, 1500);
    } catch (e) {
      alert(t('config.error_save', { name: prop.name, msg: e.message }));
    } finally {
      this._saving = new Set([...this._saving].filter(k => k !== key));
    }
  }

  _renderInput(prop) {
    const val = this._values[prop.key] ?? '';

    if (prop.property_type === 'bool') {
      const effective = val !== '' ? val : (prop.default_value ?? 'true');
      const checked   = effective !== 'false';
      return html`
        <div class="form-check form-switch config-bool-switch">
          <input class="form-check-input" type="checkbox" role="switch"
                 id="cfg-${prop.key}"
                 .checked=${checked}
                 @change=${e => { this._setValue(prop.key, e.target.checked ? 'true' : 'false'); this._save(prop); }} />
          <label class="form-check-label" for="cfg-${prop.key}">
            ${checked ? t('config.enabled') : t('config.disabled')}
          </label>
        </div>`;
    }

    if (prop.property_type === 'int') {
      return html`
        <input type="number" step="1" min="1"
               class="form-control form-control-sm config-input"
               .value=${val}
               placeholder=${prop.default_value ?? ''}
               @input=${e => this._setValue(prop.key, e.target.value)} />`;
    }

    // Dropdown-style property types. The backend ships the allowed values in
    // `prop.options` (a list of {id, name}); we only decide how to frame them.
    // Adding a new custom type from a config section? Give it a `property_type`
    // on the backend, attach its `options`, and add a branch like these — a
    // free-text box becomes a proper picker for the price of a few lines.
    if (prop.property_type === 'security_group') {
      // Nullable: the empty choice means "fall back to the instance default".
      const groups = prop.options ?? [];
      return html`
        <select class="form-select form-select-sm config-input"
                .value=${val}
                @change=${e => this._setValue(prop.key, e.target.value)}>
          <option value="">— default —</option>
          ${groups.map(g => html`
            <option value=${g.id} ?selected=${val === g.id}>${g.name}</option>`)}
        </select>`;
    }

    if (prop.property_type === 'locale') {
      // Interface languages the instance supports; labels are native endonyms.
      // Always a concrete pick (no empty option) — falls back to default_value.
      const locales = prop.options ?? [];
      const current = val || prop.default_value || 'en';
      return html`
        <select class="form-select form-select-sm config-input"
                .value=${current}
                @change=${e => { this._setValue(prop.key, e.target.value); this._save(prop); }}>
          ${locales.map(l => html`
            <option value=${l.id} ?selected=${current === l.id}>${l.name}</option>`)}
        </select>`;
    }

    return html`
      <input type="text"
             class="form-control form-control-sm config-input"
             .value=${val}
             placeholder=${prop.default_value ?? ''}
             @input=${e => this._setValue(prop.key, e.target.value)} />`;
  }

  _renderSet(set) {
    const slug = _configSetSlug(set.name);
    const sName = slug ? _maybeT(`config.set.${slug}.name`, set.name) : set.name;
    const sDesc = slug ? _maybeT(`config.set.${slug}.desc`, set.description) : set.description;
    return html`
      <div class="config-set">
        <div class="config-set-header">
          <div class="config-set-name">${sName}</div>
          <div class="config-set-desc">${sDesc}</div>
        </div>
        <div class="config-rows">
          ${set.properties.map(p => this._renderRow(p))}
        </div>
      </div>`;
  }

  _renderRow(prop) {
    const saving = this._saving.has(prop.key);
    const saved  = this._saved.has(prop.key);
    const pk = _propKeyId(prop.key);
    const pName = _maybeT(`config.prop.${pk}.name`, prop.name);
    const pDesc = _maybeT(`config.prop.${pk}.desc`, prop.description);

    return html`
      <div class="config-row">
        <div class="config-row-meta">
          <div class="config-row-name">${pName}</div>
          <div class="config-row-desc">${pDesc}</div>
        </div>
        <div class="config-row-control">
          ${this._renderInput(prop)}
          ${!['bool', 'locale'].includes(prop.property_type) ? html`
            <button class="btn btn-sm ${saved ? 'btn-success' : 'btn-primary'} config-save-btn"
                    ?disabled=${saving}
                    @click=${() => this._save(prop)}>
              ${saving
                ? html`<span class="spinner-border spinner-border-sm"></span>`
                : saved ? t('common.saved') : t('common.save')}
            </button>` : nothing}
        </div>
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
