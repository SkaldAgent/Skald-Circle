import { html } from 'lit';
import { LightElement } from '../lib/base.js';

const STRENGTH_COLORS = {
  very_high: '#ef4444',
  high:      '#f97316',
  average:   '#eab308',
  low:       '#84cc16',
  very_low:  '#22c55e',
};

const STRENGTH_LABELS = {
  very_high: 'Very High',
  high:      'High',
  average:   'Average',
  low:       'Low',
  very_low:  'Very Low',
};

const STRENGTH_OPTIONS = ['very_low', 'low', 'average', 'high', 'very_high'];
const SCOPE_OPTIONS    = ['coding', 'writing', 'reasoning', 'math', 'basic', 'search'];

function emptyMeta() {
  // `reasoning` is the selected reasoning value: a string for a ValueSet mode,
  // a number for a Range mode, or null (off). Interpreted per provider.
  return { strength: '', scope: [], priority: 100, is_default: false, reasoning: null };
}

function emptyOrForm() {
  return {
    model_id:   '',
    name:       '',
    max_tokens: '',
    ...emptyMeta(),
  };
}

function emptyDefaultForm() {
  return { model_id: '', name: '', extra_params: '', ...emptyMeta() };
}

export class ModelsLlmSection extends LightElement {
  static properties = {
    onback:          { attribute: false },
    _models:         { state: true },
    _providers:      { state: true },
    _modal:          { state: true },
    _saving:         { state: true },
    _error:          { state: true },
    _orModels:       { state: true },
    _orLoading:      { state: true },
    _orSearch:       { state: true },
    _orForm:         { state: true },
    _form:           { state: true },
    _pickedProvider: { state: true },
    _reasoningMode:  { state: true },
  };

  constructor() {
    super();
    this.onback          = null;
    this._models         = [];
    this._providers      = [];
    this._modal          = null;
    this._saving         = false;
    this._error          = null;
    this._orModels       = [];
    this._orLoading      = false;
    this._orSearch       = '';
    this._orForm         = emptyOrForm();
    this._form           = emptyDefaultForm();
    this._pickedProvider = null;
    this._reasoningMode  = null;
  }

  connectedCallback() {
    super.connectedCallback();
    this._load();
  }

  async _load() {
    try {
      const [modelsRes, providersRes] = await Promise.all([
        fetch('/api/llm/models'),
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

  // ── Move up/down ─────────────────────────────────────────────────────────────

  async _moveUp(i) {
    if (i <= 0) return;
    const models = [...this._models];
    [models[i - 1], models[i]] = [models[i], models[i - 1]];
    this._models = models;
    await this._savePriorities();
  }

  async _moveDown(i) {
    if (i >= this._models.length - 1) return;
    const models = [...this._models];
    [models[i], models[i + 1]] = [models[i + 1], models[i]];
    this._models = models;
    await this._savePriorities();
  }

  async _savePriorities() {
    try {
      await Promise.all(this._models.map((m, i) =>
        fetch(`/api/llm/models/${m.id}`, {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          // A PUT overwrites every column, so carry through all preserved
          // fields (catalog metadata + reasoning) — omitting them would wipe
          // them on reorder.
          body: JSON.stringify({
            provider_id:       m.provider_id,
            model_id:          m.model_id,
            name:              m.name,
            strength:          m.strength ?? null,
            scope:             m.scope,
            is_default:        m.is_default,
            priority:          (i + 1) * 10,
            extra_params:      m.extra_params ?? null,
            context_length:    m.context_length ?? null,
            max_output_tokens: m.max_output_tokens ?? null,
            knowledge_cutoff:  m.knowledge_cutoff ?? null,
            capabilities:      m.capabilities ?? null,
            reasoning:         m.reasoning ?? null,
          }),
        })
      ));
      await this._load();
    } catch (e) {
      this._error = `Failed to save order: ${e.message}`;
      await this._load();
    }
  }

  // ── Add flow ─────────────────────────────────────────────────────────────────

  _openAdd() {
    this._error          = null;
    this._pickedProvider = null;
    this._modal          = 'provider-pick';
  }

  async _pickProvider(provider) {
    this._error          = null;
    this._pickedProvider = provider;
    this._reasoningMode  = null;

    const hasModelPicker = ['openrouter', 'ollama', 'lm_studio', 'deepseek', 'zai'].includes(provider.type);

    if (hasModelPicker) {
      this._orForm   = emptyOrForm();
      this._orSearch = '';
      this._orModels = [];
      this._modal    = 'add-openrouter';
      await this._loadOrModels(provider.id);
    } else {
      this._form  = { ...emptyDefaultForm(), provider_id: provider.id };
      this._modal = 'add-default';
    }
  }

  async _loadOrModels(providerId) {
    this._orLoading = true;
    try {
      const res = await fetch(`/api/llm/providers/${providerId}/models`);
      if (!res.ok) throw new Error(`HTTP ${res.status}: ${await res.text()}`);
      this._orModels = await res.json();
    } catch (e) {
      this._error = `Failed to load models: ${e.message}`;
    } finally {
      this._orLoading = false;
    }
  }

  // ── Edit flow ────────────────────────────────────────────────────────────────

  async _openEdit(model) {
    this._error = null;
    try {
      const res = await fetch(`/api/llm/models/${model.id}`);
      if (!res.ok) throw new Error(`HTTP ${res.status}`);
      const record = await res.json();
      this._form = {
        strength:          record.strength ?? '',
        scope:             record.scope ?? [],
        priority:          record.priority,
        is_default:        record.is_default,
        provider_id:       record.provider_id,
        model_id:          record.model_id,
        name:              record.name,
        extra_params:      record.extra_params ? JSON.stringify(record.extra_params) : '',
        reasoning:         record.reasoning ?? null,
        // Preserved so the PUT doesn't wipe catalog metadata (see _submitEdit).
        context_length:    record.context_length ?? null,
        max_output_tokens: record.max_output_tokens ?? null,
        knowledge_cutoff:  record.knowledge_cutoff ?? null,
        capabilities:      record.capabilities ?? null,
      };
      // The reasoning control descriptor lives on the list item (LlmModelInfo).
      this._modal = {
        mode: 'edit', id: record.id, name: record.name, model_id: record.model_id,
        reasoning_mode: model.reasoning_mode ?? null,
      };
    } catch (e) {
      this._error = e.message;
    }
  }

  // ── Delete ───────────────────────────────────────────────────────────────────

  async _delete(model) {
    if (!confirm(`Delete model "${model.name}"?`)) return;
    try {
      const res = await fetch(`/api/llm/models/${model.id}`, { method: 'DELETE' });
      if (!res.ok) throw new Error(await res.text());
      await this._load();
    } catch (e) {
      this._error = e.message;
    }
  }

  // ── Submit: add default ──────────────────────────────────────────────────────

  async _submitDefault(e) {
    e.preventDefault();
    if (this._saving) return;
    this._saving = true;
    this._error  = null;

    const f = this._form;
    let extra_params = null;
    if (f.extra_params && f.extra_params.trim()) {
      try { extra_params = JSON.parse(f.extra_params); }
      catch { this._error = 'Extra params: invalid JSON'; this._saving = false; return; }
    }

    try {
      const res = await fetch('/api/llm/models', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          provider_id:  Number(this._pickedProvider.id),
          model_id:     f.model_id,
          name:         f.name || f.model_id,
          strength:     f.strength || null,
          scope:        f.scope,
          is_default:   f.is_default,
          priority:     Number(f.priority),
          extra_params,
          reasoning:    f.reasoning ?? null,
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

  // ── Submit: add openrouter ───────────────────────────────────────────────────

  async _submitCatalog(e) {
    e.preventDefault();
    if (this._saving) return;
    if (!this._orForm.model_id) { this._error = 'Select a model'; return; }
    this._saving = true;
    this._error  = null;

    const f = this._orForm;
    const extra_params = {};
    if (f.max_tokens) extra_params.max_tokens = Number(f.max_tokens);

    const selected = this._orModels.find(m => m.id === f.model_id);

    try {
      const res = await fetch('/api/llm/models', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          provider_id:       Number(this._pickedProvider.id),
          model_id:          f.model_id,
          name:              f.name || f.model_id,
          strength:          f.strength || null,
          scope:             f.scope,
          is_default:        f.is_default,
          priority:          Number(f.priority),
          extra_params:      Object.keys(extra_params).length ? extra_params : null,
          context_length:    selected?.context_length ?? null,
          max_output_tokens: selected?.max_completion_tokens ?? null,
          knowledge_cutoff:  selected?.knowledge_cutoff ?? null,
          capabilities:      selected?.capabilities?.length ? selected.capabilities : null,
          reasoning:         f.reasoning ?? null,
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

  // ── Submit: edit ─────────────────────────────────────────────────────────────

  async _submitEdit(e) {
    e.preventDefault();
    if (this._saving) return;
    this._saving = true;
    this._error  = null;

    const f = this._form;
    let extra_params = null;
    if (f.extra_params && f.extra_params.trim()) {
      try { extra_params = JSON.parse(f.extra_params); }
      catch { this._error = 'Extra params: invalid JSON'; this._saving = false; return; }
    }

    try {
      const res = await fetch(`/api/llm/models/${this._modal.id}`, {
        method: 'PUT',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          provider_id:  f.provider_id,
          model_id:     f.model_id,
          name:         f.name,
          strength:     f.strength || null,
          scope:        f.scope,
          is_default:   f.is_default,
          priority:     Number(f.priority),
          extra_params,
          reasoning:         f.reasoning ?? null,
          context_length:    f.context_length ?? null,
          max_output_tokens: f.max_output_tokens ?? null,
          knowledge_cutoff:  f.knowledge_cutoff ?? null,
          capabilities:      f.capabilities ?? null,
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

  // ── Field helpers ────────────────────────────────────────────────────────────

  _setField(field, value) {
    this._form = { ...this._form, [field]: value };
  }

  // Manual "add model" flow: the reasoning control descriptor depends on the
  // model_id, so fetch it (per provider) when the field settles.
  async _fetchDefaultReasoning(modelId) {
    this._reasoningMode = null;
    this._setField('reasoning', null);
    const pid = this._pickedProvider?.id;
    if (!pid || !modelId.trim()) return;
    try {
      const res = await fetch(`/api/llm/providers/${pid}/reasoning-mode?model_id=${encodeURIComponent(modelId)}`);
      if (res.ok) this._reasoningMode = await res.json();
    } catch { /* descriptor is optional; ignore */ }
  }

  _setOrField(field, value) {
    this._orForm = { ...this._orForm, [field]: value };
  }

  _toggleScope(scope, isOr = false) {
    if (isOr) {
      const s = this._orForm.scope;
      this._orForm = { ...this._orForm, scope: s.includes(scope) ? s.filter(x => x !== scope) : [...s, scope] };
    } else {
      const s = this._form.scope;
      this._form = { ...this._form, scope: s.includes(scope) ? s.filter(x => x !== scope) : [...s, scope] };
    }
  }

  _closeModal() { this._modal = null; this._error = null; }

  // ── Render helpers ───────────────────────────────────────────────────────────

  _fmtP(p) {
    if (p == null) return null;
    if (p === 0)   return '$0';
    return p < 0.01 ? `$${p.toFixed(4)}` : `$${p.toFixed(3)}`;
  }

  _renderPriceCell(model) {
    const inp = this._fmtP(model.price_input_per_million);
    const out = this._fmtP(model.price_output_per_million);
    if (!inp && !out) return html`<span style="opacity:0.3">—</span>`;
    return html`
      <span class="llm-price-tag" title="Input/Output per 1M tokens">
        ${inp ?? '?'} <span style="opacity:0.45">→</span> ${out ?? '?'}
      </span>
    `;
  }

  _renderStrengthDot(strength) {
    if (!strength) return html`<span style="opacity:0.3">—</span>`;
    return html`
      <span class="llm-strength-dot"
            style="background:${STRENGTH_COLORS[strength] ?? '#888'}"
            title=${STRENGTH_LABELS[strength] ?? strength}></span>
    `;
  }

  _renderStatus(status) {
    const cfg = {
      healthy:  { color: '#22c55e', title: 'Healthy' },
      degraded: { color: '#eab308', title: 'Degraded' },
      down:     { color: '#ef4444', title: 'Down' },
    }[status] ?? { color: '#888', title: status };
    return html`<span class="llm-strength-dot" style="background:${cfg.color}" title=${cfg.title}></span>`;
  }

  _renderCard(m, i) {
    const first = i === 0;
    const last  = i === this._models.length - 1;
    return html`
      <div class="llm-card">
        <div class="llm-card-row1">
          <div class="llm-move-btns">
            <button class="llm-move-btn" title="Move up"
              ?disabled=${first}
              @click=${() => this._moveUp(i)}>
              <i class="bi bi-chevron-up"></i>
            </button>
            <button class="llm-move-btn" title="Move down"
              ?disabled=${last}
              @click=${() => this._moveDown(i)}>
              <i class="bi bi-chevron-down"></i>
            </button>
          </div>
          ${this._renderStrengthDot(m.strength)}
          ${this._renderStatus(m.status)}
          <span class="llm-card-name">${m.name}</span>
          ${m.is_default ? html`<span class="llm-card-badge">default</span>` : ''}
          <div class="llm-card-actions">
            <button class="llm-btn-icon llm-btn-edit" title="Edit" @click=${() => this._openEdit(m)}>
              <i class="bi bi-pencil"></i>
            </button>
            <button class="llm-btn-icon llm-btn-delete" title="Delete" @click=${() => this._delete(m)}>
              <i class="bi bi-trash"></i>
            </button>
          </div>
        </div>

        <div class="llm-card-row2">
          <span class="llm-provider-name">${m.provider_name}</span>
          <span class="llm-model-id">${m.model_id}</span>
          ${this._renderPriceCell(m)}
        </div>

        ${(m.scope ?? []).length > 0 || m.extra_params ? html`
          <div class="llm-card-row3">
            ${(m.scope ?? []).map(s => html`<span class="llm-scope-pill">${s}</span>`)}
            ${m.extra_params ? html`<span class="llm-scope-pill llm-params-pill" title=${JSON.stringify(m.extra_params)}>+params</span>` : ''}
          </div>
        ` : ''}
      </div>
    `;
  }

  // Data-driven reasoning control. `mode` is the provider-supplied descriptor
  // (ReasoningMode): a `value_set` (discrete choices → dropdown) or a `range`
  // (numeric budget → checkbox + number input). `form.reasoning` holds the
  // current value (string for value_set, number for range, or null = off).
  _renderReasoning(form, setField, mode) {
    if (!mode) return '';
    if (mode.type === 'value_set') {
      return html`
        <div class="mb-3">
          <label class="form-label fw-semibold" style="font-size:0.82rem">Reasoning</label>
          <select class="form-select form-select-sm"
            @change=${(e) => setField('reasoning', e.target.value || null)}>
            <option value="" ?selected=${form.reasoning == null}>— off —</option>
            ${mode.values.map(v => html`
              <option value=${v} ?selected=${form.reasoning === v}>${v}</option>
            `)}
          </select>
        </div>`;
    }
    // range
    const enabled = form.reasoning != null;
    return html`
      <div class="mb-3">
        <div class="form-check mb-2">
          <input class="form-check-input" type="checkbox" id="m-reasoning-on"
            .checked=${enabled}
            @change=${(e) => setField('reasoning', e.target.checked ? (mode.default ?? mode.min) : null)} />
          <label class="form-check-label fw-semibold" for="m-reasoning-on" style="font-size:0.82rem">
            Reasoning (thinking)
          </label>
        </div>
        ${enabled ? html`
          <div class="d-flex align-items-center gap-2 ms-3">
            <input type="number" class="form-control form-control-sm" style="max-width:140px"
              min=${mode.min} max=${mode.max} step=${mode.step ?? 1}
              .value=${String(form.reasoning)}
              @input=${(e) => setField('reasoning', e.target.value === '' ? null : Number(e.target.value))} />
            <span class="text-muted" style="font-size:0.78rem">${mode.unit ?? ''}</span>
          </div>
        ` : ''}
      </div>`;
  }

  _renderMetaFields(form, setField, toggleScope, reasoningMode) {
    return html`
      ${this._renderReasoning(form, setField, reasoningMode)}
      <div class="row g-3 mb-3">
        <div class="col-8">
          <label class="form-label fw-semibold" style="font-size:0.82rem">Strength</label>
          <select class="form-select form-select-sm"
            @change=${(e) => setField('strength', e.target.value)}>
            <option value="">— none —</option>
            ${STRENGTH_OPTIONS.map(s => html`
              <option value=${s} ?selected=${form.strength === s}>${STRENGTH_LABELS[s]}</option>
            `)}
          </select>
        </div>
        <div class="col-4">
          <label class="form-label fw-semibold" style="font-size:0.82rem">Priority</label>
          <input type="number" class="form-control form-control-sm" .value=${String(form.priority)} min="1"
            @input=${(e) => setField('priority', e.target.value)} />
        </div>
      </div>

      <div class="mb-3">
        <label class="form-label fw-semibold" style="font-size:0.82rem">Scope</label>
        <div class="llm-scope-grid">
          ${SCOPE_OPTIONS.map(s => html`
            <div class="form-check">
              <input class="form-check-input" type="checkbox" id="scope-${s}"
                .checked=${form.scope.includes(s)} @change=${() => toggleScope(s)} />
              <label class="form-check-label" for="scope-${s}" style="font-size:0.82rem">${s}</label>
            </div>
          `)}
        </div>
      </div>

      <div class="mb-3">
        <div class="form-check">
          <input class="form-check-input" type="checkbox" id="m-is-default"
            .checked=${form.is_default} @change=${(e) => setField('is_default', e.target.checked)} />
          <label class="form-check-label" for="m-is-default" style="font-size:0.82rem">Default model</label>
        </div>
      </div>
    `;
  }

  // ── Modal: provider picker ───────────────────────────────────────────────────

  _renderProviderPick() {
    return html`
      <div class="agent-dialog-backdrop" @click=${(e) => { if (e.target === e.currentTarget) this._closeModal(); }}>
        <div class="agent-dialog llm-modal">
          <div class="llm-modal-title">Add Model — Choose Provider</div>
          ${this._error ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:0.85rem">${this._error}</div>` : ''}
          <div class="llm-provider-grid">
            ${this._providers.filter(p => (p.supported_types ?? []).includes('llm')).map(p => html`
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

  // ── Modal: add default ───────────────────────────────────────────────────────

  _renderAddDefault() {
    const f = this._form;
    const p = this._pickedProvider;
    return html`
      <div class="agent-dialog-backdrop" @click=${(e) => { if (e.target === e.currentTarget) this._closeModal(); }}>
        <div class="agent-dialog llm-modal">
          <div class="llm-modal-title">
            Add Model
            <span class="badge bg-secondary ms-2" style="font-size:0.7rem;font-weight:400">${p?.name}</span>
          </div>
          ${this._error ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:0.85rem">${this._error}</div>` : ''}
          <form @submit=${(e) => this._submitDefault(e)}>
            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">Model ID <span class="text-muted fw-normal">(sent to API)</span></label>
              <input type="text" class="form-control form-control-sm" .value=${f.model_id} required
                placeholder="e.g. gpt-4o"
                @input=${(e) => this._setField('model_id', e.target.value)}
                @change=${(e) => this._fetchDefaultReasoning(e.target.value)} />
            </div>
            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">Name / Alias <span class="text-muted fw-normal">(optional)</span></label>
              <input type="text" class="form-control form-control-sm" .value=${f.name}
                placeholder=${f.model_id || 'same as model ID'}
                @input=${(e) => this._setField('name', e.target.value)} />
            </div>
            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">
                Extra params <span class="text-muted fw-normal">(JSON, optional)</span>
              </label>
              <textarea class="form-control form-control-sm font-monospace" rows="3"
                .value=${f.extra_params}
                @input=${(e) => this._setField('extra_params', e.target.value)}
                style="font-size:0.78rem;resize:vertical"></textarea>
            </div>
            ${this._renderMetaFields(f, (k, v) => this._setField(k, v), (s) => this._toggleScope(s), this._reasoningMode)}
            <div class="agent-dialog-actions">
              <button type="button" class="btn btn-sm btn-secondary" @click=${() => this._closeModal()}>Cancel</button>
              <button type="submit" class="btn btn-sm btn-primary" ?disabled=${this._saving}>
                ${this._saving ? 'Saving…' : 'Add model'}
              </button>
            </div>
          </form>
        </div>
      </div>
    `;
  }

  // ── Modal: add model from catalog ────────────────────────────────────────────

  _renderAddCatalog() {
    const f      = this._orForm;
    const p      = this._pickedProvider;
    const search = this._orSearch.toLowerCase();
    const filtered = this._orModels.filter(m =>
      !search || m.id.toLowerCase().includes(search) || m.name.toLowerCase().includes(search)
    );

    const selected = this._orModels.find(m => m.id === f.model_id);
    const supportsMaxTokens = selected?.capabilities?.includes('max_tokens') ?? true;

    const formatPrice = (m) => {
      const i = this._fmtP(m.price_input_per_million);
      const o = this._fmtP(m.price_output_per_million);
      if (!i && !o) return null;
      return `${i ?? '?'} → ${o ?? '?'}/M`;
    };

    return html`
      <div class="agent-dialog-backdrop" @click=${(e) => { if (e.target === e.currentTarget) this._closeModal(); }}>
        <div class="agent-dialog llm-modal llm-modal-wide">
          <div class="llm-modal-title">
            Add Model
            <span class="badge bg-secondary ms-2" style="font-size:0.7rem;font-weight:400">${p?.name}</span>
          </div>
          ${this._error ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:0.85rem">${this._error}</div>` : ''}

          <form @submit=${(e) => this._submitCatalog(e)}>
            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">Model</label>
              <input type="text" class="form-control form-control-sm mb-1"
                placeholder="Search models…"
                .value=${this._orSearch}
                @input=${(e) => { this._orSearch = e.target.value; }} />

              ${this._orLoading
                ? html`<div class="text-muted py-2" style="font-size:0.82rem">Loading models…</div>`
                : html`
                  <div class="llm-or-model-list">
                    ${filtered.length === 0
                      ? html`<div class="text-muted px-2 py-1" style="font-size:0.82rem">No models found</div>`
                      : filtered.map(m => html`
                        <div class="llm-or-model-row ${f.model_id === m.id ? 'selected' : ''}"
                             @click=${() => this._setOrField('model_id', m.id)}>
                          <div class="llm-or-model-name">${m.name}</div>
                          <div class="llm-or-model-meta">
                            <span class="text-muted" style="font-size:0.72rem">${m.id}</span>
                            ${(m.price_input_per_million != null || m.price_output_per_million != null) ? html`
                              <span class="llm-or-price">${formatPrice(m)}</span>
                            ` : ''}
                            ${m.context_length ? html`
                              <span class="text-muted" style="font-size:0.72rem">${(m.context_length / 1000).toFixed(0)}k ctx</span>
                            ` : ''}
                          </div>
                        </div>
                      `)
                    }
                  </div>
                `
              }
            </div>

            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">Name / Alias <span class="text-muted fw-normal">(optional)</span></label>
              <input type="text" class="form-control form-control-sm" .value=${f.name}
                placeholder=${selected?.name || f.model_id || 'same as model ID'}
                @input=${(e) => this._setOrField('name', e.target.value)} />
            </div>

            ${supportsMaxTokens ? html`
              <div class="mb-3">
                <label class="form-label fw-semibold" style="font-size:0.82rem">Max output tokens <span class="text-muted fw-normal">(optional)</span></label>
                <input type="number" class="form-control form-control-sm" .value=${f.max_tokens}
                  placeholder=${selected?.max_completion_tokens ? `up to ${selected.max_completion_tokens.toLocaleString()}` : ''}
                  min="1"
                  @input=${(e) => this._setOrField('max_tokens', e.target.value)} />
              </div>
            ` : ''}

            ${this._renderMetaFields(f, (k, v) => this._setOrField(k, v), (s) => this._toggleScope(s, true), selected?.reasoning)}

            <div class="agent-dialog-actions">
              <button type="button" class="btn btn-sm btn-secondary" @click=${() => this._closeModal()}>Cancel</button>
              <button type="submit" class="btn btn-sm btn-primary" ?disabled=${this._saving || !f.model_id}>
                ${this._saving ? 'Saving…' : 'Add model'}
              </button>
            </div>
          </form>
        </div>
      </div>
    `;
  }

  // ── Modal: edit ──────────────────────────────────────────────────────────────

  _renderEdit() {
    const m = this._modal;
    const f = this._form;
    return html`
      <div class="agent-dialog-backdrop" @click=${(e) => { if (e.target === e.currentTarget) this._closeModal(); }}>
        <div class="agent-dialog llm-modal">
          <div class="llm-modal-title">
            Edit
            <span class="text-muted fw-normal ms-1" style="font-size:0.9rem">${m.name}</span>
          </div>
          <p class="text-muted mb-3" style="font-size:0.8rem">
            Model ID and provider cannot be changed. To use a different model, add a new entry.
          </p>
          ${this._error ? html`<div class="alert alert-danger py-2 mb-3" style="font-size:0.85rem">${this._error}</div>` : ''}
          <form @submit=${(e) => this._submitEdit(e)}>
            <div class="mb-3">
              <label class="form-label fw-semibold" style="font-size:0.82rem">Name / Alias</label>
              <input type="text" class="form-control form-control-sm" .value=${f.name}
                placeholder=${m.model_id || 'model name'}
                @input=${(e) => this._setField('name', e.target.value)} />
              <div class="form-text" style="font-size:0.75rem">Used to reference this model (e.g. in an agent's <code>client</code>). Must be unique.</div>
            </div>
            ${this._renderMetaFields(f, (k, v) => this._setField(k, v), (s) => this._toggleScope(s), this._modal.reasoning_mode)}
            <div class="agent-dialog-actions">
              <button type="button" class="btn btn-sm btn-secondary" @click=${() => this._closeModal()}>Cancel</button>
              <button type="submit" class="btn btn-sm btn-primary" ?disabled=${this._saving}>
                ${this._saving ? 'Saving…' : 'Save changes'}
              </button>
            </div>
          </form>
        </div>
      </div>
    `;
  }

  render() {
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
              <h2 class="llm-page-title">LLM Models</h2>
              <span class="llm-page-count">${this._models.length} model${this._models.length !== 1 ? 's' : ''}</span>
            </div>
          </div>
          <button class="btn btn-sm btn-primary" @click=${() => this._openAdd()}
            ?disabled=${this._providers.length === 0}>
            <i class="bi bi-plus-lg me-1"></i>Add
          </button>
        </div>

        ${this._providers.length === 0 ? html`
          <div class="agent-info-banner">
            <div class="agent-info-banner-icon"><i class="bi bi-info-circle-fill"></i></div>
            <div class="agent-info-banner-body">
              <p class="mb-0">Add a <strong>Provider</strong> first, then come back here to add models.</p>
            </div>
          </div>
        ` : ''}

        ${this._error && !this._modal ? html`
          <div class="alert alert-danger py-2 mx-3 mb-0" style="font-size:0.85rem">${this._error}</div>
        ` : ''}

        <div class="llm-card-list">
          ${this._models.length === 0 && this._providers.length > 0 ? html`
            <div class="llm-empty-state">
              <i class="bi bi-cpu"></i>
              <p>No models configured yet.</p>
              <button class="btn btn-sm btn-primary" @click=${() => this._openAdd()}>
                <i class="bi bi-plus-lg me-1"></i>Add your first model
              </button>
            </div>
          ` : this._models.map((m, i) => this._renderCard(m, i))}
        </div>
      </div>

      ${this._modal === 'provider-pick'  ? this._renderProviderPick() : ''}
      ${this._modal === 'add-openrouter' ? this._renderAddCatalog()   : ''}
      ${this._modal === 'add-default'    ? this._renderAddDefault()   : ''}
      ${this._modal?.mode === 'edit'     ? this._renderEdit()         : ''}
    `;
  }
}
