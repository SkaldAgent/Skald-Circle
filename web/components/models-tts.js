import { html } from 'lit';
import { LightElement } from '../lib/base.js';

// Audio formats accepted by the OpenAI-compatible `/audio/speech` endpoint.
const TTS_RESPONSE_FORMATS = ['mp3', 'opus', 'aac', 'flac', 'wav', 'pcm'];

function emptyTtsForm() {
  return { provider_id: '', model_id: '', voice_id: '', name: '', description: '', instructions: '', response_format: '', priority: 100 };
}

export class ModelsTtsSection extends LightElement {
  static properties = {
    onback:         { attribute: false },
    _models:        { state: true },
    _providers:     { state: true },
    _modal:         { state: true },
    _form:          { state: true },
    _saving:        { state: true },
    _error:         { state: true },
    _provider:      { state: true },
    _remoteModels:  { state: true },
    _loadingModels: { state: true },
  };

  constructor() {
    super();
    this.onback         = null;
    this._models        = [];
    this._providers     = [];
    this._modal         = null;
    this._form          = emptyTtsForm();
    this._saving        = false;
    this._error         = null;
    this._provider      = null;
    this._remoteModels  = null;
    this._loadingModels = false;
  }

  connectedCallback() {
    super.connectedCallback();
    this._load();
  }

  async _load() {
    try {
      const [modelsRes, providersRes] = await Promise.all([
        fetch('/api/tts/models'),
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

  // ── Add flow ──────────────────────────────────────────────────────────────────

  _openAdd() {
    this._error    = null;
    this._provider = null;
    this._form     = emptyTtsForm();
    this._modal    = 'pick-provider';
  }

  async _pickProvider(provider) {
    this._provider      = provider;
    this._remoteModels  = null;
    this._form          = { ...emptyTtsForm(), provider_id: provider.id };
    this._loadingModels = true;
    this._modal         = 'pick-model';
    try {
      const res = await fetch(`/api/tts/providers/${provider.id}/models`);
      this._remoteModels = res.ok ? await res.json() : null;
    } catch {
      this._remoteModels = null;
    } finally {
      this._loadingModels = false;
      if (!this._remoteModels || this._remoteModels.length === 0) {
        this._modal = 'add';
      }
    }
  }

  _pickRemoteModel(remote) {
    this._form  = {
      ...this._form,
      model_id:     remote.id,
      name:         remote.name,
      description:  remote.description ?? '',
      instructions: remote.instructions ?? '',
    };
    this._modal = 'add';
  }

  // ── Edit flow ─────────────────────────────────────────────────────────────────

  async _openEdit(m) {
    this._error = null;
    try {
      const res = await fetch(`/api/tts/models/${m.id}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const r = await res.json();
      this._provider = this._providers.find(p => p.id === r.provider_id) ?? null;
      this._form = {
        provider_id:  r.provider_id,
        model_id:     r.model_id,
        voice_id:     r.voice_id     ?? '',
        name:            r.name,
        description:     r.description     ?? '',
        instructions:    r.instructions    ?? '',
        response_format: r.response_format ?? '',
        priority:        r.priority,
      };
      this._modal = { mode: 'edit', id: r.id, name: r.name };
    } catch (e) {
      this._error = e.message;
    }
  }

  // ── Delete ────────────────────────────────────────────────────────────────────

  async _delete(m) {
    if (!confirm(`Delete TTS model "${m.name}"?`)) return;
    try {
      const res = await fetch(`/api/tts/models/${m.id}`, { method: 'DELETE' });
      if (!res.ok) throw new Error(await res.text());
      await this._load();
    } catch (e) {
      this._error = e.message;
    }
  }

  // ── Submit ────────────────────────────────────────────────────────────────────

  _payload() {
    const f = this._form;
    return {
      provider_id:  Number(f.provider_id),
      model_id:     f.model_id,
      voice_id:     f.voice_id     || null,
      name:         f.name || f.model_id,
      description:  f.description  || null,
      instructions: f.instructions || null,
      response_format: f.response_format || null,
      priority:     Number(f.priority) || 100,
    };
  }

  async _submitAdd(e) {
    e.preventDefault();
    if (this._saving) return;
    this._saving = true;
    this._error  = null;
    try {
      const res = await fetch('/api/tts/models', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(this._payload()),
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

  async _submitEdit(e) {
    e.preventDefault();
    if (this._saving) return;
    this._saving = true;
    this._error  = null;
    const id = this._modal.id;
    try {
      const res = await fetch(`/api/tts/models/${id}`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(this._payload()),
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

  // ── Render card ───────────────────────────────────────────────────────────────

  _renderCard(m) {
    const isPlugin = m.from_plugin;
    return html`
      <div class="llm-card">
        <div class="llm-card-row1">
          ${isPlugin
            ? html`<span class="ig-source-badge ig-source-plugin">Plugin</span>`
            : html`<span class="ig-source-badge ig-source-cloud">Cloud</span>`}
          <span class="llm-card-name">${m.name}</span>
          <div class="llm-card-actions">
            ${isPlugin ? html`
              <span class="llm-btn-icon" title="Managed by plugin" style="cursor:default;opacity:0.4">
                <i class="bi bi-lock"></i>
              </span>
            ` : html`
              <button class="llm-btn-icon llm-btn-edit" title="Edit" @click=${() => this._openEdit(m)}>
                <i class="bi bi-pencil"></i>
              </button>
              <button class="llm-btn-icon llm-btn-delete" title="Delete" @click=${() => this._delete(m)}>
                <i class="bi bi-trash"></i>
              </button>
            `}
          </div>
        </div>

        <div class="llm-card-row2">
          ${!isPlugin ? html`<span class="llm-provider-name">${m.provider_name}</span>` : ''}
          <span class="llm-model-id">${isPlugin ? m.model_id || m.id : m.model_id}</span>
          ${m.voice_id ? html`<span class="llm-model-id" style="opacity:0.6" title="Voice ID">${m.voice_id}</span>` : ''}
          ${m.response_format ? html`<span class="llm-model-id" style="opacity:0.6" title="Response format">${m.response_format}</span>` : ''}
          ${!isPlugin ? html`<span class="ig-priority-tag" title="Priority">#${m.priority}</span>` : ''}
        </div>

        ${m.description ? html`
          <div class="ig-card-desc">${m.description}</div>
        ` : ''}

        ${m.instructions ? html`
          <div class="ig-card-desc" style="font-style:italic;color:var(--bs-secondary)">${m.instructions}</div>
        ` : ''}
      </div>
    `;
  }

  // ── Modal: pick provider ──────────────────────────────────────────────────────

  _renderPickProvider() {
    const ttsProviders = this._providers.filter(p =>
      Array.isArray(p.supported_types) && p.supported_types.includes('tts')
    );
    return html`
      <div class="agent-dialog-backdrop" @click=${(e) => { if (e.target === e.currentTarget) this._closeModal(); }}>
        <div class="agent-dialog llm-modal">
          <div class="llm-modal-title">Add TTS Model — Choose Provider</div>
          ${this._error ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:0.85rem">${this._error}</div>` : ''}
          <div class="llm-provider-grid">
            ${ttsProviders.map(p => html`
              <button class="llm-provider-card" @click=${() => this._pickProvider(p)}>
                <div class="llm-provider-card-name">${p.name}</div>
                <div class="llm-provider-card-type text-muted" style="font-size:0.75rem">${p.type}</div>
              </button>
            `)}
          </div>
          <div class="agent-dialog-actions mt-3">
            <button type="button" class="btn btn-sm btn-secondary" @click=${() => this._closeModal()}>Cancel</button>
          </div>
        </div>
      </div>
    `;
  }

  // ── Modal: pick remote model ──────────────────────────────────────────────────

  _renderPickModel() {
    const p = this._provider;
    return html`
      <div class="agent-dialog-backdrop" @click=${(e) => { if (e.target === e.currentTarget) this._closeModal(); }}>
        <div class="agent-dialog llm-modal">
          <div class="llm-modal-title">
            Add TTS Model
            <span class="badge bg-secondary ms-2" style="font-size:0.7rem;font-weight:400">${p?.name}</span>
          </div>
          ${this._loadingModels ? html`
            <div class="text-center py-4 text-muted" style="font-size:0.85rem">
              <div class="spinner-border spinner-border-sm me-2"></div>Loading models…
            </div>
          ` : html`
            <div class="tts-model-pick-list">
              ${(this._remoteModels ?? []).map(m => html`
                <button class="tts-model-pick-item" @click=${() => this._pickRemoteModel(m)}>
                  <div class="tts-model-pick-row1">
                    <span class="tts-model-pick-name">${m.name}</span>
                    ${m.cost_factor != null ? html`
                      <span class="tts-model-pick-cost" title="Cost multiplier relative to base rate">×${m.cost_factor.toFixed(1)}</span>
                    ` : ''}
                  </div>
                  ${m.description ? html`<div class="tts-model-pick-desc">${m.description}</div>` : ''}
                  ${m.languages?.length ? html`
                    <div class="tts-model-pick-langs">${m.languages.slice(0, 6).join(', ')}${m.languages.length > 6 ? ` +${m.languages.length - 6}` : ''}</div>
                  ` : ''}
                </button>
              `)}
            </div>
            <div class="agent-dialog-actions mt-3">
              <button type="button" class="btn btn-sm btn-secondary" @click=${() => this._closeModal()}>Cancel</button>
              <button type="button" class="btn btn-sm btn-outline-secondary" @click=${() => { this._modal = 'add'; }}>
                Enter model ID manually
              </button>
            </div>
          `}
        </div>
      </div>
    `;
  }

  // ── Modal: add / edit form ────────────────────────────────────────────────────

  _renderForm(isEdit = false) {
    const f = this._form;
    const p = this._provider;
    const title = isEdit
      ? html`Edit <span class="text-muted fw-normal ms-1" style="font-size:0.9rem">${this._modal.name}</span>`
      : html`Add TTS Model <span class="badge bg-secondary ms-2" style="font-size:0.7rem;font-weight:400">${p?.name}</span>`;

    return html`
      <div class="agent-dialog-backdrop" @click=${(e) => { if (e.target === e.currentTarget) this._closeModal(); }}>
        <div class="agent-dialog llm-modal">
          <div class="llm-modal-title">${title}</div>
          ${this._error ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:0.85rem">${this._error}</div>` : ''}
          <form @submit=${(e) => isEdit ? this._submitEdit(e) : this._submitAdd(e)}>

            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">
                Model ID <span class="text-muted fw-normal">(sent to API)</span>
              </label>
              <input type="text" class="form-control form-control-sm" .value=${f.model_id} required
                placeholder="e.g. tts-1-hd"
                ?disabled=${isEdit}
                @input=${(e) => this._form = { ...this._form, model_id: e.target.value }} />
              ${isEdit ? html`<div class="form-text">Model ID cannot be changed after creation.</div>` : ''}
            </div>

            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">
                Voice ID <span class="text-muted fw-normal">(optional — required for ElevenLabs)</span>
              </label>
              <input type="text" class="form-control form-control-sm" .value=${f.voice_id}
                placeholder="e.g. alloy, Kore, 21m00Tcm4TlvDq8ikWAM"
                @input=${(e) => this._form = { ...this._form, voice_id: e.target.value }} />
              <div class="form-text">
                Speaker voice. OpenAI: <code>alloy</code>/<code>echo</code>/<code>nova</code>… (default <code>alloy</code> if empty);
                Gemini: <code>Kore</code>/<code>Puck</code>/<code>Zephyr</code>…; ElevenLabs: the voice ID.
              </div>
            </div>

            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">Name / Alias</label>
              <input type="text" class="form-control form-control-sm" .value=${f.name}
                placeholder=${f.model_id || 'same as model ID'}
                @input=${(e) => this._form = { ...this._form, name: e.target.value }} />
            </div>

            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">
                Description <span class="text-muted fw-normal">(optional)</span>
              </label>
              <input type="text" class="form-control form-control-sm" .value=${f.description}
                placeholder="e.g. High quality, slow — best for long responses"
                @input=${(e) => this._form = { ...this._form, description: e.target.value }} />
            </div>

            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">
                Instructions <span class="text-muted fw-normal">(optional — shown to LLM)</span>
              </label>
              <textarea class="form-control form-control-sm" rows="3" .value=${f.instructions}
                placeholder="e.g. Speak in a calm, neutral tone. Pause slightly between sentences."
                @input=${(e) => this._form = { ...this._form, instructions: e.target.value }}></textarea>
              <div class="form-text">Voice/tone guidance injected into the LLM system prompt when this model is active.</div>
            </div>

            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">
                Response format <span class="text-muted fw-normal">(optional)</span>
              </label>
              <select class="form-select form-select-sm" .value=${f.response_format}
                @change=${(e) => this._form = { ...this._form, response_format: e.target.value }}>
                <option value="">Provider default (mp3)</option>
                ${TTS_RESPONSE_FORMATS.map(fmt => html`
                  <option value=${fmt}>${fmt}</option>
                `)}
              </select>
              <div class="form-text">
                Audio format requested from the provider. Leave empty unless the model requires
                a specific one — e.g. Gemini TTS only accepts <code>pcm</code>.
              </div>
            </div>

            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">Priority</label>
              <input type="number" class="form-control form-control-sm" .value=${String(f.priority)} min="1"
                @input=${(e) => this._form = { ...this._form, priority: e.target.value }} />
              <div class="form-text">Lower number = used first. Default: 100.</div>
            </div>

            <div class="agent-dialog-actions">
              <button type="button" class="btn btn-sm btn-secondary" @click=${() => this._closeModal()}>Cancel</button>
              <button type="submit" class="btn btn-sm btn-primary" ?disabled=${this._saving}>
                ${this._saving ? 'Saving…' : isEdit ? 'Save changes' : 'Add model'}
              </button>
            </div>
          </form>
        </div>
      </div>
    `;
  }

  render() {
    const ttsProviders = this._providers.filter(p =>
      Array.isArray(p.supported_types) && p.supported_types.includes('tts')
    );
    const canAdd = ttsProviders.length > 0;

    return html`
      <div class="llm-page">
        <div class="llm-page-header">
          <div class="llm-header-left">
            ${this.onback ? html`
              <button class="btn btn-sm btn-outline-secondary back-btn" title="Back to models" @click=${this.onback}>
                <i class="bi bi-arrow-left"></i>
              </button>
            ` : ''}
            <div>
              <h2 class="llm-page-title">Text-to-Speech Models</h2>
              <span class="llm-page-count">${this._models.length} model${this._models.length !== 1 ? 's' : ''}</span>
            </div>
          </div>
          <button class="btn btn-sm btn-primary" @click=${() => this._openAdd()} ?disabled=${!canAdd}>
            <i class="bi bi-plus-lg me-1"></i>Add
          </button>
        </div>

        ${!canAdd ? html`
          <div class="agent-info-banner">
            <div class="agent-info-banner-icon"><i class="bi bi-info-circle-fill"></i></div>
            <div class="agent-info-banner-body">
              <p class="mb-0">No provider supports TTS yet. Add an <strong>OpenAI</strong> provider first.</p>
            </div>
          </div>
        ` : ''}

        ${this._models.some(m => m.from_plugin) ? html`
          <div class="agent-info-banner">
            <div class="agent-info-banner-icon"><i class="bi bi-info-circle-fill"></i></div>
            <div class="agent-info-banner-body">
              <p class="mb-0">Models with the <strong>Plugin</strong> badge are read-only — managed automatically by the plugin that registered them.</p>
            </div>
          </div>
        ` : ''}

        ${this._error && !this._modal ? html`
          <div class="alert alert-danger py-2 mx-3 mb-0" style="font-size:0.85rem">${this._error}</div>
        ` : ''}

        <div class="llm-card-list">
          ${this._models.length === 0 ? html`
            <div class="llm-empty-state">
              <i class="bi bi-volume-up"></i>
              <p>No TTS models configured.</p>
              ${canAdd ? html`
                <button class="btn btn-sm btn-primary" @click=${() => this._openAdd()}>
                  <i class="bi bi-plus-lg me-1"></i>Add your first model
                </button>
              ` : ''}
            </div>
          ` : this._models.map(m => this._renderCard(m))}
        </div>
      </div>

      ${this._modal === 'pick-provider' ? this._renderPickProvider() : ''}
      ${this._modal === 'pick-model'    ? this._renderPickModel()    : ''}
      ${this._modal === 'add'           ? this._renderForm(false)    : ''}
      ${this._modal?.mode === 'edit'    ? this._renderForm(true)     : ''}
    `;
  }
}
