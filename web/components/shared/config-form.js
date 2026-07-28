import { html, nothing } from 'lit';
import { t }             from '../../lib/i18n.js';

/**
 * The schema-driven settings form, shared by the Config page and the System
 * agents page.
 *
 * A config property renders the same way wherever it is edited — the backend
 * ships its type, its current value and, for the dropdown types, the choices it
 * owns (`PropertyType` in `core-api`), and this decides how to frame them. Both
 * pages write through the same `PUT /api/config/{key}`, so "where is this
 * setting shown" stays a pure placement question with no second form to keep in
 * step.
 *
 * Adding a property type is still the three-step recipe in `config_property.rs`:
 * variant, backend mapping + options, and a branch in `_renderInput` here.
 */
export class ConfigFormController {
  /** @param requestUpdate host callback, invoked whenever state changes. */
  constructor(requestUpdate) {
    this._requestUpdate = requestUpdate;
    this._values = {};
    this._saving = new Set();
    this._saved  = new Set();
  }

  /** Seed the editable values from freshly fetched sets. */
  seedFromSets(sets) {
    const vals = {};
    for (const s of sets ?? [])
      for (const p of s?.properties ?? []) vals[p.key] = p.value ?? '';
    this._values = vals;
    this._requestUpdate();
  }

  _setValue(key, val) {
    this._values = { ...this._values, [key]: val };
    this._requestUpdate();
  }

  async _save(prop) {
    const key   = prop.key;
    const value = this._values[key] ?? '';

    this._saving = new Set([...this._saving, key]);
    this._requestUpdate();

    try {
      const res = await fetch(`/api/config/${encodeURIComponent(key)}`, {
        method:  'PUT',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify({ value }),
      });
      if (!res.ok) throw new Error(await res.text());

      this._saved = new Set([...this._saved, key]);
      this._requestUpdate();
      setTimeout(() => {
        this._saved = new Set([...this._saved].filter(k => k !== key));
        this._requestUpdate();
      }, 1500);
    } catch (e) {
      alert(t('config.error_save', { name: prop.name, msg: e.message }));
    } finally {
      this._saving = new Set([...this._saving].filter(k => k !== key));
      this._requestUpdate();
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
    if (prop.property_type === 'security_group') {
      // Nullable: the empty choice means "fall back to the role default".
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

    if (prop.property_type === 'llm_model') {
      // Configured LLM models, by name. Nullable: the empty choice means
      // "auto-select" (the backend's own resolution order applies).
      const models = prop.options ?? [];
      return html`
        <select class="form-select form-select-sm config-input"
                .value=${val}
                @change=${e => this._setValue(prop.key, e.target.value)}>
          <option value="">— ${t('config.llm_model.auto')} —</option>
          ${models.map(m => html`
            <option value=${m.id} ?selected=${val === m.id}>${m.name}</option>`)}
        </select>`;
    }

    return html`
      <input type="text"
             class="form-control form-control-sm config-input"
             .value=${val}
             placeholder=${prop.default_value ?? ''}
             @input=${e => this._setValue(prop.key, e.target.value)} />`;
  }

  /**
   * One row per property. `labelFor(prop)` lets the host supply translated
   * name/description; it returns `{ name, description }`.
   */
  renderRows(properties, labelFor = p => p) {
    return html`
      <div class="config-rows">
        ${(properties ?? []).map(prop => {
          const saving = this._saving.has(prop.key);
          const saved  = this._saved.has(prop.key);
          const label  = labelFor(prop);
          // A switch and a language picker save on change; everything else needs
          // an explicit commit, or every keystroke would be a write.
          const needsButton = !['bool', 'locale'].includes(prop.property_type);
          return html`
            <div class="config-row">
              <div class="config-row-meta">
                <div class="config-row-name">${label.name}</div>
                <div class="config-row-desc">${label.description}</div>
              </div>
              <div class="config-row-control">
                ${this._renderInput(prop)}
                ${needsButton ? html`
                  <button class="btn btn-sm ${saved ? 'btn-success' : 'btn-primary'} config-save-btn"
                          ?disabled=${saving}
                          @click=${() => this._save(prop)}>
                    ${saving
                      ? html`<span class="spinner-border spinner-border-sm"></span>`
                      : saved ? t('common.saved') : t('common.save')}
                  </button>` : nothing}
              </div>
            </div>`;
        })}
      </div>`;
  }
}

/** `t(key)` when a translation exists, otherwise the server-supplied text. */
export function maybeT(key, fallback) {
  const v = t(key);
  return v !== key ? v : fallback;
}

/** Config keys are dotted; i18n keys are not. */
export function propKeyId(propKey) {
  return propKey.replace(/\./g, '__');
}
