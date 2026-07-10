import { html } from 'lit';
import { LightElement } from '../lib/base.js';

function emptyTForm() {
  return { provider_id: '', model_id: '', name: '', language: '', priority: 100 };
}

export class ModelsTranscribeSection extends LightElement {
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
    this._form          = emptyTForm();
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
        fetch('/api/transcribe/models'),
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
    this._form     = emptyTForm();
    this._modal    = 'pick-provider';
  }

  async _pickProvider(provider) {
    this._provider      = provider;
    this._remoteModels  = null;
    this._form          = { ...emptyTForm(), provider_id: provider.id };
    this._loadingModels = true;
    this._modal         = 'pick-model';
    try {
      const res = await fetch(`/api/transcribe/providers/${provider.id}/models`);
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
      model_id: remote.id,
      name:     remote.name,
    };
    this._modal = 'add';
  }

  // ── Edit flow ────────────────────────────────────────────────────────────────

  async _openEdit(m) {
    this._error = null;
    try {
      const res = await fetch(`/api/transcribe/models/${m.id}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const r = await res.json();
      this._provider = this._providers.find(p => p.id === r.provider_id) ?? null;
      this._form = {
        provider_id: r.provider_id,
        model_id:    r.model_id,
        name:        r.name,
        language:    r.language ?? '',
        priority:    r.priority,
      };
      this._modal = { mode: 'edit', id: r.id, name: r.name };
    } catch (e) {
      this._error = e.message;
    }
  }

  // ── Delete ───────────────────────────────────────────────────────────────────

  async _delete(m) {
    if (!confirm(`Delete transcription model "${m.name}"?`)) return;
    try {
      const res = await fetch(`/api/transcribe/models/${m.id}`, { method: 'DELETE' });
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
      const res = await fetch('/api/transcribe/models', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          provider_id: Number(f.provider_id),
          model_id:    f.model_id,
          name:        f.name || f.model_id,
          language:    f.language || null,
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
      const res = await fetch(`/api/transcribe/models/${id}`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          provider_id: Number(f.provider_id),
          model_id:    f.model_id,
          name:        f.name || f.model_id,
          language:    f.language || null,
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

  // ── Render ───────────────────────────────────────────────────────────────────

  _renderRow(m) {
    const isPlugin = m.from_plugin;
    return html`
      <tr class="llm-row">
        <td>
          ${isPlugin
            ? html`<span class="badge" style="background:#7c3aed;font-size:0.65rem;font-weight:500">Plugin</span>`
            : html`<span class="badge bg-secondary" style="font-size:0.65rem;font-weight:500">Cloud</span>`}
        </td>
        <td><span class="fw-semibold">${m.name}</span></td>
        <td class="text-muted" style="font-size:0.8rem">${isPlugin ? '—' : m.provider_name}</td>
        <td class="llm-model" title=${m.model_id}>${m.model_id}</td>
        <td style="font-size:0.8rem">${m.language ?? html`<span style="opacity:0.35">auto</span>`}</td>
        <td class="llm-actions">
          ${isPlugin ? html`
            <span class="text-muted" style="font-size:0.75rem" title="Managed by plugin">
              <i class="bi bi-lock"></i>
            </span>
          ` : html`
            <button class="btn btn-sm btn-link" title="Edit" @click=${() => this._openEdit(m)}>
              <i class="bi bi-pencil"></i>
            </button>
            <button class="btn btn-sm btn-link text-danger" title="Delete" @click=${() => this._delete(m)}>
              <i class="bi bi-trash"></i>
            </button>
          `}
        </td>
      </tr>
    `;
  }

  _renderPickProvider() {
    const tProviders = this._providers.filter(p =>
      Array.isArray(p.supported_types) && p.supported_types.includes('transcribe')
    );
    return html`
      <div class="agent-dialog-backdrop" @click=${(e) => { if (e.target === e.currentTarget) this._closeModal(); }}>
        <div class="agent-dialog llm-modal">
          <div class="llm-modal-title">Add Transcription Model — Choose Provider</div>
          ${this._error ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:0.85rem">${this._error}</div>` : ''}
          <div class="llm-provider-grid">
            ${tProviders.map(p => html`
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

  _renderPickModel() {
    const p = this._provider;
    return html`
      <div class="agent-dialog-backdrop" @click=${(e) => { if (e.target === e.currentTarget) this._closeModal(); }}>
        <div class="agent-dialog llm-modal">
          <div class="llm-modal-title">
            Add Transcription Model
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
                  <div class="tts-model-pick-name">${m.name}</div>
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

  _renderForm(isEdit = false) {
    const f = this._form;
    const p = this._provider;
    const title = isEdit
      ? html`Edit <span class="text-muted fw-normal ms-1" style="font-size:0.9rem">${this._modal.name}</span>`
      : html`Add Transcription Model <span class="badge bg-secondary ms-2" style="font-size:0.7rem;font-weight:400">${p?.name}</span>`;

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
                placeholder="e.g. openai/whisper-1"
                ?disabled=${isEdit}
                @input=${(e) => this._form = { ...this._form, model_id: e.target.value }} />
              ${isEdit ? html`<div class="form-text">Model ID cannot be changed after creation.</div>` : ''}
            </div>

            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">
                Name / Alias <span class="text-muted fw-normal">(optional)</span>
              </label>
              <input type="text" class="form-control form-control-sm" .value=${f.name}
                placeholder=${f.model_id || 'same as model ID'}
                @input=${(e) => this._form = { ...this._form, name: e.target.value }} />
            </div>

            <div class="row g-3 mb-3">
              <div class="col-8">
                <label class="form-label fw-semibold" style="font-size:0.82rem">
                  Language <span class="text-muted fw-normal">(BCP-47, optional)</span>
                </label>
                <input type="text" class="form-control form-control-sm" .value=${f.language}
                  placeholder="e.g. it, en  — leave blank for auto-detect"
                  @input=${(e) => this._form = { ...this._form, language: e.target.value }} />
              </div>
              <div class="col-4">
                <label class="form-label fw-semibold" style="font-size:0.82rem">Priority</label>
                <input type="number" class="form-control form-control-sm" .value=${String(f.priority)} min="1"
                  @input=${(e) => this._form = { ...this._form, priority: e.target.value }} />
              </div>
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
    const tProviders = this._providers.filter(p =>
      Array.isArray(p.supported_types) && p.supported_types.includes('transcribe')
    );
    const canAdd = tProviders.length > 0;

    return html`
      <div class="llm-page">
        <div class="llm-page-header">
          <div class="llm-header-left">
            ${this.onback ? html`
              <button class="btn btn-sm btn-outline-secondary back-btn" title="Back to models" @click=${this.onback}>
                <i class="bi bi-arrow-left"></i>
              </button>
            ` : ''}
            <h2 class="llm-page-title">Transcription Models</h2>
          </div>
          <button class="btn btn-sm btn-primary" @click=${() => this._openAdd()} ?disabled=${!canAdd}>
            <i class="bi bi-plus-lg me-1"></i>Add
          </button>
        </div>

        ${!canAdd ? html`
          <div class="agent-info-banner">
            <div class="agent-info-banner-icon"><i class="bi bi-info-circle-fill"></i></div>
            <div class="agent-info-banner-body">
              <p class="mb-0">No provider supports transcription yet. Add an <strong>OpenAI</strong> or <strong>OpenRouter</strong> provider first.</p>
            </div>
          </div>
        ` : ''}

        ${this._error && !this._modal ? html`
          <div class="alert alert-danger py-2 mb-3" style="font-size:0.85rem">${this._error}</div>
        ` : ''}

        ${this._models.length === 0 ? html`
          <p class="text-muted" style="font-size:0.9rem">
            No transcription models configured.
            ${canAdd ? html`Click <strong>Add</strong> to add a cloud model.` : ''}
            Activate the <strong>Whisper Local</strong> plugin for on-device transcription.
          </p>
        ` : html`
          <div class="table-responsive">
            <table class="table llm-table mb-0">
              <thead>
                <tr>
                  <th style="width:5rem">Source</th>
                  <th>Name</th>
                  <th>Provider</th>
                  <th>Model ID</th>
                  <th>Language</th>
                  <th></th>
                </tr>
              </thead>
              <tbody>
                ${this._models.map(m => this._renderRow(m))}
              </tbody>
            </table>
          </div>
        `}
      </div>

      ${this._modal === 'pick-provider' ? this._renderPickProvider() : ''}
      ${this._modal === 'pick-model'    ? this._renderPickModel()    : ''}
      ${this._modal === 'add'           ? this._renderForm(false)    : ''}
      ${this._modal?.mode === 'edit'    ? this._renderForm(true)     : ''}
    `;
  }
}
