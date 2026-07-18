import { html } from 'lit';
import { LightElement } from '../lib/base.js';
import { t }            from '../lib/i18n.js';

function emptyForm(firstTypeId = '') {
  return { name: '', type: firstTypeId, api_key: '', base_url: '', description: '' };
}

export class LlmProvidersPage extends LightElement {
  static properties = {
    _open:          { state: true },
    _providers:     { state: true },
    _providerTypes: { state: true },
    _modelCounts:   { state: true },
    _modal:         { state: true },
    _saving:        { state: true },
    _error:         { state: true },
    _form:          { state: true },
  };

  constructor() {
    super();
    this._open          = false;
    this._providers     = [];
    this._providerTypes = [];
    this._modelCounts   = {};
    this._modal         = null;
    this._saving        = false;
    this._error         = null;
    this._form          = emptyForm();
  }

  _typeMeta(typeId) {
    return this._providerTypes.find(t => t.type_id === typeId) ?? { display_name: typeId, color: '#888', icon: 'bi-box', fields: [] };
  }

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
    window.addEventListener('llm-page-change', (e) => {
      this._open = e.detail.page === 'providers';
      this.style.display = this._open ? 'flex' : 'none';
      if (this._open) this._load();
    });
  }

  disconnectedCallback() {
    window.removeEventListener('locale-changed', this.__onLocaleChanged);
    super.disconnectedCallback();
  }

  async _load() {
    try {
      const [typesRes, provRes, modelsRes] = await Promise.all([
        fetch('/api/llm/providers/types'),
        fetch('/api/llm/providers'),
        fetch('/api/llm/models'),
      ]);
      if (!typesRes.ok)  throw new Error(`HTTP ${typesRes.status}`);
      if (!provRes.ok)   throw new Error(`HTTP ${provRes.status}`);
      if (!modelsRes.ok) throw new Error(`HTTP ${modelsRes.status}`);

      const providerTypes = await typesRes.json();
      const providers     = await provRes.json();
      const models        = await modelsRes.json();

      const counts = {};
      for (const m of models) {
        const pid = String(m.provider_id);
        counts[pid] = (counts[pid] || 0) + 1;
      }

      this._providerTypes = providerTypes;
      this._providers     = providers;
      this._modelCounts   = counts;

      // Set default form type to first available provider type
      if (!this._form.type && providerTypes.length > 0) {
        this._form = { ...this._form, type: providerTypes[0].type_id };
      }
    } catch (e) {
      this._error = e.message;
    }
  }

  // ── CRUD ──────────────────────────────────────────────────────────────────

  _openAdd() {
    this._error = null;
    this._form  = emptyForm(this._providerTypes[0]?.type_id ?? '');
    this._modal = { mode: 'add' };
  }

  async _openEdit(provider) {
    this._error = null;
    try {
      const res = await fetch(`/api/llm/providers/${provider.id}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const record = await res.json();
      this._form = {
        name:        record.name,
        type:        record.type,
        api_key:     record.api_key  ?? '',
        base_url:    record.base_url ?? '',
        description: record.description ?? '',
      };
      this._modal = { mode: 'edit', id: record.id };
    } catch (e) {
      this._error = e.message;
    }
  }

  async _delete(provider) {
    if (!confirm(t('providers.confirm.delete', { name: provider.name }))) return;
    try {
      const res = await fetch(`/api/llm/providers/${provider.id}`, { method: 'DELETE' });
      if (!res.ok) throw new Error(await res.text());
      await this._load();
    } catch (e) {
      this._error = e.message;
    }
  }

  async _onSubmit(e) {
    e.preventDefault();
    if (this._saving) return;
    this._saving = true;
    this._error  = null;

    const f = this._form;
    const meta = this._typeMeta(f.type);
    const needsBaseUrl = meta.fields.some(field => field.key === 'base_url');
    const payload = {
      name:        f.name,
      type:        f.type,
      api_key:     f.api_key     || null,
      base_url:    needsBaseUrl ? (f.base_url || null) : null,
      description: f.description || null,
    };

    const isEdit = this._modal?.mode === 'edit';
    const url    = isEdit ? `/api/llm/providers/${this._modal.id}` : '/api/llm/providers';

    try {
      const res = await fetch(url, {
        method:  isEdit ? 'PUT' : 'POST',
        headers: { 'Content-Type': 'application/json' },
        body:    JSON.stringify(payload),
      });
      if (!res.ok) throw new Error(await res.text());
      this._modal = null;
      await this._load();
    } catch (err) {
      this._error = err.message;
    } finally {
      this._saving = false;
    }
  }

  _setField(field, value) {
    this._form = { ...this._form, [field]: value };
  }

  _closeModal() { this._modal = null; this._error = null; }

  // ── Render helpers ────────────────────────────────────────────────────────

  _renderCard(p) {
    const meta     = this._typeMeta(p.type);
    const color    = meta.color;
    const icon     = meta.icon;
    const label    = meta.display_name;
    const count    = this._modelCounts[String(p.id)];
    const hasKey   = Boolean(p.api_key);
    const needsUrl = meta.fields.some(f => f.key === 'base_url');

    return html`
      <div class="pv-card" style="--pv-color: ${color}">
        <div class="pv-card-row1">
          <div class="pv-card-icon" style="background: color-mix(in srgb, ${color} 14%, transparent); color: ${color}">
            <i class="bi ${icon}"></i>
          </div>
          <span class="pv-card-name">${p.name}</span>
          <span class="pv-card-type-badge">${label}</span>
          ${count != null ? html`
            <span class="pv-card-count" title=${t('providers.card.models_title')}>
              <i class="bi bi-cpu me-1"></i>${count}
            </span>
          ` : ''}
          <div class="pv-card-actions">
            <button class="pv-btn-icon pv-btn-edit" title=${t('providers.card.edit')} @click=${() => this._openEdit(p)}>
              <i class="bi bi-pencil"></i>
            </button>
            <button class="pv-btn-icon pv-btn-delete" title=${t('providers.card.delete')} @click=${() => this._delete(p)}>
              <i class="bi bi-trash"></i>
            </button>
          </div>
        </div>

        ${p.description ? html`
          <div class="pv-card-row2">
            <span class="pv-card-desc">${p.description}</span>
          </div>
        ` : ''}

        <div class="pv-card-row3">
          <span class="pv-card-tag ${hasKey ? 'pv-tag-ok' : 'pv-tag-missing'}">
            <i class="bi ${hasKey ? 'bi-lock-fill' : 'bi-unlock'}"></i>
            ${hasKey ? t('providers.card.api_key_configured') : t('providers.card.api_key_missing')}
          </span>
          ${needsUrl && p.base_url ? html`
            <span class="pv-card-tag pv-tag-url" title=${t('providers.card.base_url')}>
              <i class="bi bi-link-45deg"></i>
              <span class="pv-card-url-text">${p.base_url}</span>
            </span>
          ` : ''}
          ${p.created_at ? html`
            <span class="pv-card-tag pv-tag-date">
              <i class="bi bi-calendar3"></i>
              ${new Date(p.created_at).toLocaleDateString()}
            </span>
          ` : ''}
        </div>
      </div>
    `;
  }

  // ── Modal ─────────────────────────────────────────────────────────────────

  _renderModal() {
    const isEdit   = this._modal?.mode === 'edit';
    const f        = this._form;
    const meta     = this._typeMeta(f.type);
    const needsKey = meta.fields.some(field => field.key === 'api_key');
    const needsUrl = meta.fields.some(field => field.key === 'base_url');

    return html`
      <div class="agent-dialog-backdrop" @click=${(e) => { if (e.target === e.currentTarget) this._closeModal(); }}>
        <div class="agent-dialog pv-modal">
          <div class="pv-modal-header">
            <i class="bi bi-plug"></i>
            <span>${isEdit ? t('providers.modal.edit') : t('providers.modal.add')}</span>
            <button type="button" class="pv-modal-close" @click=${() => this._closeModal()}>
              <i class="bi bi-x"></i>
            </button>
          </div>

          ${this._error ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:0.85rem">${this._error}</div>` : ''}

          <form @submit=${(e) => this._onSubmit(e)}>
            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">${t('providers.modal.name')}</label>
              <input type="text" class="form-control form-control-sm" .value=${f.name} required
                placeholder=${t('providers.modal.name_ph')} @input=${(e) => this._setField('name', e.target.value)} />
            </div>

            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">${t('providers.modal.type')}</label>
              <select class="form-select form-select-sm" .value=${f.type}
                @change=${(e) => this._setField('type', e.target.value)}>
                ${this._providerTypes.map(t => html`<option value=${t.type_id}>${t.display_name}</option>`)}
              </select>
            </div>

            ${needsKey ? html`
              <div class="mb-3">
                <label class="form-label fw-semibold" style="font-size:0.82rem">${t('providers.modal.api_key')}</label>
                <input type="password" class="form-control form-control-sm" .value=${f.api_key}
                  autocomplete="new-password"
                  placeholder=${isEdit ? t('providers.modal.api_key_ph') : ''}
                  @input=${(e) => this._setField('api_key', e.target.value)} />
              </div>
            ` : ''}

            ${needsUrl ? html`
              <div class="mb-3">
                <label class="form-label fw-semibold" style="font-size:0.82rem">${t('providers.modal.base_url')}</label>
                <input type="text" class="form-control form-control-sm" .value=${f.base_url}
                  placeholder=${f.type === 'ollama' ? t('providers.modal.base_url_ollama') : t('providers.modal.base_url_oai')}
                  @input=${(e) => this._setField('base_url', e.target.value)} />
              </div>
            ` : ''}

            <div class="mb-4">
              <label class="form-label fw-semibold" style="font-size:0.82rem">${t('providers.modal.description')} <span class="text-muted fw-normal">${t('providers.modal.description_optional')}</span></label>
              <input type="text" class="form-control form-control-sm" .value=${f.description}
                @input=${(e) => this._setField('description', e.target.value)} />
            </div>

            <div class="pv-modal-actions">
              <button type="button" class="btn btn-sm btn-outline-secondary" @click=${() => this._closeModal()}>${t('providers.modal.cancel')}</button>
              <button type="submit" class="btn btn-sm btn-primary" ?disabled=${this._saving}>
                ${this._saving
                  ? html`<span class="spinner-border spinner-border-sm me-1"></span>${t('providers.modal.saving')}`
                  : html`<i class="bi bi-check-lg me-1"></i>${isEdit ? t('providers.modal.save_changes') : t('providers.modal.add_provider')}`}
              </button>
            </div>
          </form>
        </div>
      </div>
    `;
  }

  // ── Main render ───────────────────────────────────────────────────────────

  render() {
    return html`
      <div class="pv-page">
        <div class="pv-header">
          <h2 class="pv-title">
            <i class="bi bi-plug me-2"></i>${t('providers.title')}
          </h2>
          <div class="pv-header-right">
            <span class="pv-header-count">${t('providers.count', { n: this._providers.length })}</span>
            <button class="btn btn-sm btn-primary" @click=${() => this._openAdd()}>
              <i class="bi bi-plus-lg me-1"></i>${t('providers.add')}
          </div>
        </div>

        ${this._error && !this._modal ? html`
          <div class="alert alert-danger py-2 mx-3 mb-0" style="font-size:0.85rem">${this._error}</div>
        ` : ''}

        <div class="pv-card-list">
          ${this._providers.length === 0 ? html`
            <div class="pv-empty">
              <i class="bi bi-plug"></i>
              <p>${t('providers.empty')}</p>
              <button class="btn btn-sm btn-primary" @click=${() => this._openAdd()}>
                <i class="bi bi-plus-lg me-1"></i>${t('providers.add_first')}
              </button>
            </div>
          ` : this._providers.map(p => this._renderCard(p))}
        </div>
      </div>

      ${this._modal ? this._renderModal() : ''}
    `;
  }
}
