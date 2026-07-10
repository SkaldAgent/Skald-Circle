import { html, nothing } from 'lit';
import { LightElement } from '../lib/base.js';

export class ConfigPage extends LightElement {
  static properties = {
    _open:       { state: true },
    _properties: { state: true },
    _values:     { state: true },   // { [key]: string }
    _saving:     { state: true },   // Set<key>
    _saved:      { state: true },   // Set<key>  (brief flash)
    _error:      { state: true },
  };

  constructor() {
    super();
    this._open       = false;
    this._properties = [];
    this._values     = {};
    this._saving     = new Set();
    this._saved      = new Set();
    this._error      = null;
  }

  connectedCallback() {
    super.connectedCallback();
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'config';
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) this._load();
    });
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
      alert(`Error saving ${prop.name}: ${e.message}`);
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
            ${checked ? 'Enabled' : 'Disabled'}
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

    if (prop.property_type === 'security_group') {
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

    return html`
      <input type="text"
             class="form-control form-control-sm config-input"
             .value=${val}
             placeholder=${prop.default_value ?? ''}
             @input=${e => this._setValue(prop.key, e.target.value)} />`;
  }

  _renderSet(set) {
    return html`
      <div class="config-set">
        <div class="config-set-header">
          <div class="config-set-name">${set.name}</div>
          <div class="config-set-desc">${set.description}</div>
        </div>
        <div class="config-rows">
          ${set.properties.map(p => this._renderRow(p))}
        </div>
      </div>`;
  }

  _renderRow(prop) {
    const saving = this._saving.has(prop.key);
    const saved  = this._saved.has(prop.key);

    return html`
      <div class="config-row">
        <div class="config-row-meta">
          <div class="config-row-name">${prop.name}</div>
          <div class="config-row-desc">${prop.description}</div>
        </div>
        <div class="config-row-control">
          ${this._renderInput(prop)}
          ${prop.property_type !== 'bool' ? html`
            <button class="btn btn-sm ${saved ? 'btn-success' : 'btn-primary'} config-save-btn"
                    ?disabled=${saving}
                    @click=${() => this._save(prop)}>
              ${saving
                ? html`<span class="spinner-border spinner-border-sm"></span>`
                : saved ? 'Saved' : 'Save'}
            </button>` : nothing}
        </div>
      </div>`;
  }

  render() {
    return html`
      <div class="config-page">
        <div class="config-page-header">
          <h2 class="llm-page-title">Config</h2>
        </div>

        ${this._error ? html`
          <div class="alert alert-danger">${this._error}</div>` : nothing}

        ${this._properties.length === 0 && !this._error ? html`
          <p class="text-muted mt-2">Loading…</p>` : nothing}

        <div class="config-sets">
          ${this._properties.map(s => this._renderSet(s))}
        </div>
      </div>`;
  }
}
