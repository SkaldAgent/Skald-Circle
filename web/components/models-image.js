import { html } from 'lit';
import { unsafeHTML } from 'lit/directives/unsafe-html.js';
import { LightElement } from '../lib/base.js';
import { t }             from '../lib/i18n.js';

function emptyIgForm() {
  return { provider_id: '', model_id: '', name: '', priority: 100 };
}

export class ModelsImageSection extends LightElement {
  static properties = {
    onback:    { attribute: false },
    _models:   { state: true },
    _providers: { state: true },
    _modal:    { state: true },
    _form:     { state: true },
    _saving:   { state: true },
    _error:    { state: true },
    _provider: { state: true },
  };

  constructor() {
    super();
    this.onback     = null;
    this._models    = [];
    this._providers = [];
    this._modal     = null;
    this._form      = emptyIgForm();
    this._saving    = false;
    this._error     = null;
    this._provider  = null;
  }

  connectedCallback() {
    super.connectedCallback();
    this.__onLocaleChanged = () => this.requestUpdate();
    window.addEventListener('locale-changed', this.__onLocaleChanged);
    this._load();
  }

  disconnectedCallback() {
    window.removeEventListener('locale-changed', this.__onLocaleChanged);
    super.disconnectedCallback();
  }

  async _load() {
    try {
      const [modelsRes, providersRes] = await Promise.all([
        fetch('/api/image-generate/models'),
        fetch('/api/llm/providers'),
      ]);
      if (!modelsRes.ok)    throw new Error(`models: HTTP ${modelsRes.status}`);
      if (!providersRes.ok) throw new Error(`providers: HTTP ${providersRes.status}`);
      this._models    = await modelsRes.json();
      this._providers = await providersRes.json();
    } catch (e) {
      this._error = e.message;
    }
  }

  // ── Add flow ─────────────────────────────────────────────────────────────────

  _openAdd() {
    this._error    = null;
    this._provider = null;
    this._form     = emptyIgForm();
    this._modal    = 'pick-provider';
  }

  _pickProvider(provider) {
    this._provider = provider;
    this._form     = { ...emptyIgForm(), provider_id: provider.id };
    this._modal    = 'add';
  }

  // ── Edit flow ────────────────────────────────────────────────────────────────

  async _openEdit(m) {
    this._error = null;
    try {
      const res = await fetch(`/api/image-generate/models/${m.id}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const r = await res.json();
      this._provider = this._providers.find(p => p.id === r.provider_id) ?? null;
      this._form = {
        provider_id: r.provider_id,
        model_id:    r.model_id,
        name:        r.name,
        priority:    r.priority,
      };
      this._modal = { mode: 'edit', id: r.id, name: r.name };
    } catch (e) {
      this._error = e.message;
    }
  }

  // ── Delete ───────────────────────────────────────────────────────────────────

  async _delete(m) {
    if (!confirm(t('models.confirm_delete', { type: t('models.hub.card.image.title'), name: m.name }))) return;
    try {
      const res = await fetch(`/api/image-generate/models/${m.id}`, { method: 'DELETE' });
      if (!res.ok) throw new Error(await res.text());
      await this._load();
    } catch (e) {
      this._error = e.message;
    }
  }

  // ── Submit add ───────────────────────────────────────────────────────────────

  async _submitAdd(e) {
    e.preventDefault();
    if (this._saving) return;
    this._saving = true;
    this._error  = null;
    const f = this._form;
    try {
      const res = await fetch('/api/image-generate/models', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          provider_id: Number(f.provider_id),
          model_id:    f.model_id,
          name:        f.name || f.model_id,
          priority:    Number(f.priority) || 100,
        }),
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

  // ── Submit edit ──────────────────────────────────────────────────────────────

  async _submitEdit(e) {
    e.preventDefault();
    if (this._saving) return;
    this._saving = true;
    this._error  = null;
    const f  = this._form;
    const id = this._modal.id;
    try {
      const res = await fetch(`/api/image-generate/models/${id}`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          provider_id: Number(f.provider_id),
          model_id:    f.model_id,
          name:        f.name || f.model_id,
          priority:    Number(f.priority) || 100,
        }),
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

  _closeModal() { this._modal = null; this._error = null; }

  // ── Render card ──────────────────────────────────────────────────────────────

  _renderCard(m) {
    const isPlugin = m.from_plugin;
    return html`
      <div class="llm-card">
        <div class="llm-card-row1">
          ${isPlugin
            ? html`<span class="ig-source-badge ig-source-plugin">${t('models.source_plugin')}</span>`
            : html`<span class="ig-source-badge ig-source-cloud">${t('models.source_cloud')}</span>`}
          <span class="llm-card-name">${m.name}</span>
          <div class="llm-card-actions">
            ${isPlugin ? html`
              <span class="llm-btn-icon" title=${t('models.managed_plugin')} style="cursor:default;opacity:0.4">
                <i class="bi bi-lock"></i>
              </span>
            ` : html`
              <button class="llm-btn-icon llm-btn-edit" title=${t('models.edit')} @click=${() => this._openEdit(m)}>
                <i class="bi bi-pencil"></i>
              </button>
              <button class="llm-btn-icon llm-btn-delete" title=${t('models.delete')} @click=${() => this._delete(m)}>
                <i class="bi bi-trash"></i>
              </button>
            `}
          </div>
        </div>

        <div class="llm-card-row2">
          ${!isPlugin ? html`<span class="llm-provider-name">${m.provider_name}</span>` : ''}
          <span class="llm-model-id">${isPlugin ? m.model_id || m.id : m.model_id}</span>
          <span class="ig-priority-tag" title=${t('models.priority')}>#${m.priority}</span>
        </div>

        ${m.description ? html`
          <div class="ig-card-desc">${m.description}</div>
        ` : ''}
      </div>
    `;
  }

  // ── Modal: pick provider ──────────────────────────────────────────────────────

  _renderPickProvider() {
    const igProviders = this._providers.filter(p =>
      Array.isArray(p.supported_types) && p.supported_types.includes('image_generate')
    );
    return html`
      <div class="agent-dialog-backdrop" @click=${(e) => { if (e.target === e.currentTarget) this._closeModal(); }}>
        <div class="agent-dialog llm-modal">
          <div class="llm-modal-title">${t('models.add_model_provider', { type: t('models.hub.card.image.title') })}</div>
          ${this._error ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:0.85rem">${this._error}</div>` : ''}
          <div class="llm-provider-grid">
            ${igProviders.map(p => html`
              <button class="llm-provider-card" @click=${() => this._pickProvider(p)}>
                <div class="llm-provider-card-name">${p.name}</div>
                <div class="llm-provider-card-type text-muted" style="font-size:0.75rem">${p.type}</div>
              </button>
            `)}
          </div>
          <div class="agent-dialog-actions mt-3">
            <button type="button" class="btn btn-sm btn-secondary" @click=${() => this._closeModal()}>${t('models.cancel')}</button>
          </div>
        </div>
      </div>
    `;
  }

  // ── Modal: add / edit form ────────────────────────────────────────────────────

  _renderForm(isEdit = false) {
    const f = this._form;
    const p = this._provider;
    const title = isEdit
      ? html`${t('models.edit')} <span class="text-muted fw-normal ms-1" style="font-size:0.9rem">${this._modal.name}</span>`
      : html`${t('models.add_model_type', { type: t('models.hub.card.image.title') })} <span class="badge bg-secondary ms-2" style="font-size:0.7rem;font-weight:400">${p?.name}</span>`;

    return html`
      <div class="agent-dialog-backdrop" @click=${(e) => { if (e.target === e.currentTarget) this._closeModal(); }}>
        <div class="agent-dialog llm-modal">
          <div class="llm-modal-title">${title}</div>
          ${this._error ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:0.85rem">${this._error}</div>` : ''}
          <form @submit=${(e) => isEdit ? this._submitEdit(e) : this._submitAdd(e)}>

            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">
                ${t('models.model_id')} <span class="text-muted fw-normal">${t('models.label.sent_to_api')}</span>
              </label>
              <input type="text" class="form-control form-control-sm" .value=${f.model_id} required
                placeholder=${t('models.ph.model_id_image')}
                ?disabled=${isEdit}
                @input=${(e) => this._form = { ...this._form, model_id: e.target.value }} />
              ${isEdit ? html`<div class="form-text">${t('models.form.model_lock')}</div>` : ''}
            </div>

            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">
                ${unsafeHTML(t('models.form.name_as_provider'))}
              </label>
              <input type="text" class="form-control form-control-sm" .value=${f.name}
                placeholder=${f.model_id || t('models.ph.name_alias')}
                @input=${(e) => this._form = { ...this._form, name: e.target.value }} />
            </div>

            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">${t('models.priority')}</label>
              <input type="number" class="form-control form-control-sm" .value=${String(f.priority)} min="1"
                @input=${(e) => this._form = { ...this._form, priority: e.target.value }} />
              <div class="form-text">${t('models.form.priority_img')}</div>
            </div>

            <div class="agent-dialog-actions">
              <button type="button" class="btn btn-sm btn-secondary" @click=${() => this._closeModal()}>${t('models.cancel')}</button>
              <button type="submit" class="btn btn-sm btn-primary" ?disabled=${this._saving}>
                ${this._saving ? t('models.saving') : isEdit ? t('models.save_changes') : t('models.add_model')}
              </button>
            </div>
          </form>
        </div>
      </div>
    `;
  }

  render() {
    const igProviders = this._providers.filter(p =>
      Array.isArray(p.supported_types) && p.supported_types.includes('image_generate')
    );
    const canAdd = igProviders.length > 0;

    return html`
      <div class="llm-page">
        <div class="page-header">
          <div class="page-header-left">
            ${this.onback ? html`
              <button class="btn btn-sm btn-outline-secondary page-header-back" title=${t('models.back')} @click=${this.onback}>
                <i class="bi bi-arrow-left"></i>
              </button>
            ` : ''}
            <div>
              <h2 class="page-header-title">${t('models.image.title')}</h2>
              <span class="page-header-count">${t('models.hub.count.many', { n: this._models.length })}</span>
            </div>
          </div>
          <div class="page-header-actions">
            <button class="btn btn-sm btn-primary" @click=${() => this._openAdd()} ?disabled=${!canAdd}>
              <i class="bi bi-plus-lg me-1"></i>${t('models.add')}
            </button>
          </div>
        </div>

        ${!canAdd ? html`
          <div class="agent-info-banner">
            <div class="agent-info-banner-icon"><i class="bi bi-info-circle-fill"></i></div>
            <div class="agent-info-banner-body">
              <p class="mb-0">${t('models.no_providers_image')}</p>
            </div>
          </div>
        ` : ''}

        ${this._models.some(m => m.from_plugin) ? html`
          <div class="agent-info-banner">
            <div class="agent-info-banner-icon"><i class="bi bi-info-circle-fill"></i></div>
            <div class="agent-info-banner-body">
              <p class="mb-0">${t('models.readonly_plugin_full')}</p>
            </div>
          </div>
        ` : ''}

        ${this._error && !this._modal ? html`
          <div class="alert alert-danger py-2 mx-3 mb-0" style="font-size:0.85rem">${this._error}</div>
        ` : ''}

        <div class="llm-card-list">
          ${this._models.length === 0 ? html`
            <div class="llm-empty-state">
              <i class="bi bi-image"></i>
              <p>${t('models.list_empty_image')}</p>
              ${canAdd ? html`
                <button class="btn btn-sm btn-primary" @click=${() => this._openAdd()}>
                  <i class="bi bi-plus-lg me-1"></i>${t('models.add_first')}
                </button>
              ` : ''}
            </div>
          ` : this._models.map(m => this._renderCard(m))}
        </div>
      </div>

      ${this._modal === 'pick-provider'  ? this._renderPickProvider() : ''}
      ${this._modal === 'add'            ? this._renderForm(false)    : ''}
      ${this._modal?.mode === 'edit'     ? this._renderForm(true)     : ''}
    `;
  }
}
